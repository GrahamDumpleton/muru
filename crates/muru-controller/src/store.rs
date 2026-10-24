//! The state store: SQLite in WAL mode in the state directory, holding
//! desired state, release records and events (design specification section
//! 10.2). Every access goes through one connection behind a mutex; the
//! store is small and the reconciler is the only writer.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use muru_config::{Host as VirtualHost, Route};

use crate::model::{App, Event, Release, ReleaseSpec, ReleaseState, now_unix_ms};

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite said no.
    #[error("state store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A record could not be decoded.
    #[error("state store: {0}")]
    Decode(String),
    /// The object does not exist.
    #[error("{0} does not exist")]
    NotFound(String),
}

/// The schema, applied on open; additive changes get a new version.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS schema (version INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS apps (
    name TEXT PRIMARY KEY,
    current_release TEXT,
    previous_release TEXT,
    created_unix_ms INTEGER NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS releases (
    app TEXT NOT NULL REFERENCES apps(name),
    id TEXT NOT NULL,
    spec TEXT NOT NULL,
    state TEXT NOT NULL,
    message TEXT NOT NULL DEFAULT '',
    created_unix_ms INTEGER NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (app, id)
);
CREATE TABLE IF NOT EXISTS hosts (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS routes (
    host TEXT NOT NULL,
    path TEXT NOT NULL,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (host, path)
);
CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    time_unix_ms INTEGER NOT NULL,
    type TEXT NOT NULL,
    subject TEXT NOT NULL,
    level TEXT NOT NULL,
    message TEXT NOT NULL
);
";

/// How many events are kept.
const EVENT_RETENTION: u64 = 10_000;

/// The schema this build writes.
const SCHEMA_VERSION: i64 = 2;

/// The store.
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    /// Open or create the store at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;

        // Version 1 had applications, releases and events; version 2
        // added hosts and routes, which the CREATE statements above supply
        // to a version 1 store, so the version only needs recording.

        let version: Option<i64> = connection
            .query_row("SELECT version FROM schema LIMIT 1", [], |row| row.get(0))
            .optional()?;

        match version {
            None => {
                connection.execute("INSERT INTO schema (version) VALUES (?1)", [SCHEMA_VERSION])?;
            }
            Some(version) if version < SCHEMA_VERSION => {
                connection.execute("UPDATE schema SET version = ?1", [SCHEMA_VERSION])?;
            }
            Some(_) => {}
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// An in-memory store, for tests and for development runs.
    pub fn in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(SCHEMA)?;
        connection.execute("INSERT INTO schema (version) VALUES (?1)", [SCHEMA_VERSION])?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn with<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let connection = self.connection.lock().expect("store poisoned");
        work(&connection)
    }

    /// Every application, by name.
    pub fn apps(&self) -> Result<Vec<App>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT name, current_release, previous_release, created_unix_ms, resource_version \
                 FROM apps ORDER BY name",
            )?;
            let rows = statement.query_map([], app_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// One application.
    pub fn app(&self, name: &str) -> Result<App, StoreError> {
        self.with(|connection| {
            connection
                .query_row(
                    "SELECT name, current_release, previous_release, created_unix_ms, resource_version \
                     FROM apps WHERE name = ?1",
                    [name],
                    app_from_row,
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("application {name}")))
        })
    }

    /// Create an application if it does not exist, and return it.
    pub fn ensure_app(&self, name: &str) -> Result<App, StoreError> {
        self.with(|connection| {
            connection.execute(
                "INSERT OR IGNORE INTO apps (name, created_unix_ms) VALUES (?1, ?2)",
                params![name, now_unix_ms() as i64],
            )?;
            Ok(())
        })?;
        self.app(name)
    }

    /// Record which release serves the application and which a rollback restores.
    pub fn set_app_releases(
        &self,
        name: &str,
        current: Option<&str>,
        previous: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "UPDATE apps SET current_release = ?2, previous_release = ?3, \
                 resource_version = resource_version + 1 WHERE name = ?1",
                params![name, current, previous],
            )?;
            Ok(())
        })
    }

    /// Releases of one application, or of every application, newest first.
    pub fn releases(&self, app: Option<&str>) -> Result<Vec<Release>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT app, id, spec, state, message, created_unix_ms, resource_version \
                 FROM releases WHERE (?1 IS NULL OR app = ?1) ORDER BY created_unix_ms DESC, id DESC",
            )?;
            let rows = statement.query_map([app], release_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })
    }

    /// One release.
    pub fn release(&self, app: &str, id: &str) -> Result<Release, StoreError> {
        self.with(|connection| {
            connection
                .query_row(
                    "SELECT app, id, spec, state, message, created_unix_ms, resource_version \
                     FROM releases WHERE app = ?1 AND id = ?2",
                    [app, id],
                    release_from_row,
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("release {app}@{id}")))
        })
    }

    /// Record a new release in the pending state.
    pub fn create_release(
        &self,
        app: &str,
        id: &str,
        spec: &ReleaseSpec,
    ) -> Result<Release, StoreError> {
        let encoded =
            toml::to_string(spec).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.with(|connection| {
            connection.execute(
                "INSERT INTO releases (app, id, spec, state, created_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![app, id, encoded, ReleaseState::Pending.as_str(), now_unix_ms() as i64],
            )?;
            Ok(())
        })?;
        self.release(app, id)
    }

    /// The next generated release id for an application: r1, r2, ...
    pub fn next_release_id(&self, app: &str) -> Result<String, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare("SELECT id FROM releases WHERE app = ?1")?;
            let highest = statement
                .query_map([app], |row| row.get::<_, String>(0))?
                .filter_map(Result::ok)
                .filter_map(|id| {
                    id.strip_prefix('r')
                        .and_then(|rest| rest.parse::<u64>().ok())
                })
                .max()
                .unwrap_or(0);
            Ok(format!("r{}", highest + 1))
        })
    }

    /// Remove a release record. The caller makes sure it is not running.
    pub fn delete_release(&self, app: &str, id: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "DELETE FROM releases WHERE app = ?1 AND id = ?2",
                params![app, id],
            )?;
            Ok(())
        })
    }

    /// Remove an application and its release records. The caller makes
    /// sure none of them is running.
    pub fn delete_app(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute("DELETE FROM releases WHERE app = ?1", [name])?;
            connection.execute("DELETE FROM apps WHERE name = ?1", [name])?;
            Ok(())
        })
    }

    /// Every virtual host, by name, with its definition.
    pub fn hosts(&self) -> Result<Vec<(String, VirtualHost)>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare("SELECT name, spec FROM hosts ORDER BY name")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut hosts = Vec::new();

            for row in rows {
                let (name, encoded) = row?;
                hosts.push((name, decode(&encoded)?));
            }

            Ok(hosts)
        })
    }

    /// Create or replace a virtual host.
    pub fn put_host(&self, name: &str, host: &VirtualHost) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(host).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.with(|connection| {
            connection.execute(
                "INSERT INTO hosts (name, spec) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
                params![name, encoded],
            )?;
            Ok(())
        })
    }

    /// Remove a virtual host and its routes.
    pub fn delete_host(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute("DELETE FROM routes WHERE host = ?1", [name])?;
            connection.execute("DELETE FROM hosts WHERE name = ?1", [name])?;
            Ok(())
        })
    }

    /// Every route, by host then path.
    pub fn routes(&self) -> Result<Vec<Route>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT spec FROM routes ORDER BY host, path")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            let mut routes = Vec::new();

            for row in rows {
                routes.push(decode(&row?)?);
            }

            Ok(routes)
        })
    }

    /// Create or replace a route, identified by its host and path.
    pub fn put_route(&self, route: &Route) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(route).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.with(|connection| {
            connection.execute(
                "INSERT INTO routes (host, path, spec) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(host, path) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
                params![route.host, route.path, encoded],
            )?;
            Ok(())
        })
    }

    /// Remove a route.
    pub fn delete_route(&self, host: &str, path: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "DELETE FROM routes WHERE host = ?1 AND path = ?2",
                params![host, path],
            )?;
            Ok(())
        })
    }

    /// The sequence number of the last event: a version of the whole
    /// store, since every change to it is an event.
    pub fn state_version(&self) -> Result<u64, StoreError> {
        self.with(|connection| {
            let last: Option<i64> =
                connection.query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))?;
            Ok(last.unwrap_or(0) as u64)
        })
    }

    /// Move a release to a state, with a message saying why or what next.
    pub fn set_release_state(
        &self,
        app: &str,
        id: &str,
        state: ReleaseState,
        message: &str,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "UPDATE releases SET state = ?3, message = ?4, resource_version = resource_version + 1 \
                 WHERE app = ?1 AND id = ?2",
                params![app, id, state.as_str(), message],
            )?;
            Ok(())
        })
    }

    /// Append an event and return it with its sequence number. Events past
    /// the retention count are dropped, oldest first.
    pub fn append_event(
        &self,
        kind: &str,
        subject: &str,
        level: &'static str,
        message: &str,
    ) -> Result<Event, StoreError> {
        let time_unix_ms = now_unix_ms();
        self.with(|connection| {
            connection.execute(
                "INSERT INTO events (time_unix_ms, type, subject, level, message) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![time_unix_ms as i64, kind, subject, level, message],
            )?;
            let sequence = connection.last_insert_rowid() as u64;
            connection.execute(
                "DELETE FROM events WHERE sequence <= ?1",
                [sequence.saturating_sub(EVENT_RETENTION) as i64],
            )?;

            Ok(Event {
                sequence,
                time_unix_ms,
                kind: kind.to_owned(),
                subject: subject.to_owned(),
                level,
                message: message.to_owned(),
            })
        })
    }

    /// Events after a sequence number, oldest first, up to `limit`.
    pub fn events_after(&self, after: u64, limit: usize) -> Result<Vec<Event>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT sequence, time_unix_ms, type, subject, level, message FROM events \
                 WHERE sequence > ?1 ORDER BY sequence LIMIT ?2",
            )?;
            let rows = statement.query_map(params![after as i64, limit as i64], |row| {
                Ok(Event {
                    sequence: row.get::<_, i64>(0)? as u64,
                    time_unix_ms: row.get::<_, i64>(1)? as u64,
                    kind: row.get(2)?,
                    subject: row.get(3)?,
                    level: level_from(row.get::<_, String>(4)?),
                    message: row.get(5)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }
}

fn level_from(text: String) -> &'static str {
    match text.as_str() {
        "notice" => "notice",
        "warning" => "warning",
        "error" => "error",
        _ => "info",
    }
}

fn app_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<App> {
    Ok(App {
        name: row.get(0)?,
        current: row.get(1)?,
        previous: row.get(2)?,
        created_unix_ms: row.get::<_, i64>(3)? as u64,
        resource_version: row.get::<_, i64>(4)? as u64,
    })
}

/// A record that cannot be decoded is reported as a conversion failure
/// of its column, which is how rusqlite carries such errors.
fn undecodable(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

/// A stored definition, as TOML, back as its type.
fn decode<T: serde::de::DeserializeOwned>(encoded: &str) -> Result<T, StoreError> {
    toml::from_str(encoded).map_err(|error| StoreError::Decode(error.to_string()))
}

fn release_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Release> {
    let encoded: String = row.get(2)?;
    let spec: ReleaseSpec = toml::from_str(&encoded).map_err(|error| undecodable(2, error))?;
    let state: String = row.get(3)?;
    let state = state
        .parse::<ReleaseState>()
        .map_err(|error| undecodable(3, std::io::Error::other(error)))?;

    Ok(Release {
        app: row.get(0)?,
        id: row.get(1)?,
        spec,
        state,
        message: row.get(4)?,
        created_unix_ms: row.get::<_, i64>(5)? as u64,
        resource_version: row.get::<_, i64>(6)? as u64,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use muru_config::{Interface, Preload};

    fn spec() -> ReleaseSpec {
        ReleaseSpec {
            source: PathBuf::from("/srv/shop"),
            entrypoint: "shop.wsgi:application".to_owned(),
            interface: Interface::Auto,
            python: PathBuf::from("/srv/shop/.venv/bin/python"),
            processes: 2,
            threads: 4,
            sync_threads: 8,
            concurrency: 1000,
            preload: Preload::Auto,
            hooks: None,
            factory: None,
            groups: Vec::new(),
            mounts: Vec::new(),
            logging: Default::default(),
        }
    }

    #[test]
    fn releases_are_created_and_advance() {
        let store = Store::in_memory().unwrap();
        store.ensure_app("shop").unwrap();
        assert_eq!(store.next_release_id("shop").unwrap(), "r1");

        let release = store.create_release("shop", "r1", &spec()).unwrap();
        assert_eq!(release.state, ReleaseState::Pending);
        assert_eq!(release.spec, spec());
        assert_eq!(store.next_release_id("shop").unwrap(), "r2");

        store
            .set_release_state("shop", "r1", ReleaseState::Ready, "")
            .unwrap();
        store.set_app_releases("shop", Some("r1"), None).unwrap();
        assert_eq!(
            store.release("shop", "r1").unwrap().state,
            ReleaseState::Ready
        );
        assert_eq!(store.app("shop").unwrap().current.as_deref(), Some("r1"));
        assert!(store.release("shop", "r9").is_err());
    }

    #[test]
    fn events_are_numbered_and_listed_after_a_sequence() {
        let store = Store::in_memory().unwrap();
        let first = store
            .append_event("release.ready", "shop@r1", "info", "ready")
            .unwrap();
        let second = store
            .append_event("release.failed", "shop@r2", "error", "no")
            .unwrap();

        assert_eq!(second.sequence, first.sequence + 1);
        let after = store.events_after(first.sequence, 10).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].kind, "release.failed");
        assert_eq!(after[0].level, "error");
    }
}
