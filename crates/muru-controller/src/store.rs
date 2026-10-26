//! The state store: SQLite in WAL mode in the state directory, holding
//! desired state, release records and events (design specification section
//! 10.2). Every access goes through one connection behind a mutex; the
//! store is small and the reconciler is the only writer.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use muru_config::{Host as VirtualHost, Overlay, Route, Upstream};

use crate::model::{
    App, Event, OverlayState, Release, ReleaseSpec, ReleaseState, Rollout, now_unix_ms,
};

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
CREATE TABLE IF NOT EXISTS rollouts (
    app TEXT NOT NULL,
    id TEXT NOT NULL,
    spec TEXT NOT NULL,
    state TEXT NOT NULL,
    created INTEGER NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (app, id)
);
CREATE TABLE IF NOT EXISTS scale_pins (
    app TEXT NOT NULL,
    release TEXT NOT NULL,
    grp TEXT NOT NULL,
    processes INTEGER NOT NULL,
    PRIMARY KEY (app, release, grp)
);
CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    time_unix_ms INTEGER NOT NULL,
    type TEXT NOT NULL,
    subject TEXT NOT NULL,
    level TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS event_sinks (
    name TEXT PRIMARY KEY,
    delivered INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS maintenance (
    app TEXT PRIMARY KEY,
    since_unix_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS upstreams (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS overlays (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS overlay_state (
    name TEXT PRIMARY KEY,
    active INTEGER NOT NULL,
    source TEXT NOT NULL,
    since_unix_ms INTEGER NOT NULL,
    until_unix_ms INTEGER
);
";

/// How many events are kept.
const EVENT_RETENTION: u64 = 10_000;

/// The schema this build writes.
const SCHEMA_VERSION: i64 = 7;

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
        add_identity_columns(&connection)?;

        // Version 1 had applications, releases and events; version 2
        // added hosts and routes and version 3 the event sink cursors,
        // which the CREATE statements above supply to an older store;
        // version 4 added the application's user, group and umask, which
        // are added to an older table here; version 5 added the
        // applications in maintenance mode (section 4.12), version 6 the
        // HTTP upstreams and version 7 overlays and their state (section
        // 9.10).

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
        add_identity_columns(&connection)?;
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
                "SELECT name, current_release, previous_release, created_unix_ms, resource_version, \
                 user, grp, umask FROM apps ORDER BY name",
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
                    "SELECT name, current_release, previous_release, created_unix_ms, resource_version, \
                     user, grp, umask FROM apps WHERE name = ?1",
                    [name],
                    app_from_row,
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("application {name}")))
        })
    }

    /// Set the user, group and umask an application's processes run with
    /// (section 10.9).
    pub fn set_app_identity(
        &self,
        name: &str,
        user: Option<&str>,
        group: Option<&str>,
        umask: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            let changed = connection.execute(
                "UPDATE apps SET user = ?2, grp = ?3, umask = ?4, \
                 resource_version = resource_version + 1 WHERE name = ?1",
                params![name, user, group, umask],
            )?;

            if changed == 0 {
                return Err(StoreError::NotFound(format!("application {name}")));
            }

            Ok(())
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
            connection.execute("DELETE FROM maintenance WHERE app = ?1", [name])?;
            connection.execute("DELETE FROM apps WHERE name = ?1", [name])?;
            Ok(())
        })
    }

    /// Section 4.12: the applications in maintenance mode, each with when
    /// it was turned on.
    pub fn maintenance(&self) -> Result<Vec<(String, u64)>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT app, since_unix_ms FROM maintenance ORDER BY app")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// Put an application into maintenance mode, or take it out.
    pub fn set_maintenance(&self, app: &str, since_unix_ms: Option<u64>) -> Result<(), StoreError> {
        self.with(|connection| {
            match since_unix_ms {
                Some(since) => {
                    connection.execute(
                        "INSERT INTO maintenance (app, since_unix_ms) VALUES (?1, ?2) \
                         ON CONFLICT(app) DO NOTHING",
                        params![app, since as i64],
                    )?;
                }
                None => {
                    connection.execute("DELETE FROM maintenance WHERE app = ?1", [app])?;
                }
            }

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

    /// Every HTTP upstream, by name (section 4.13).
    pub fn upstreams(&self) -> Result<Vec<(String, Upstream)>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT name, spec FROM upstreams ORDER BY name")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut upstreams = Vec::new();

            for row in rows {
                let (name, encoded) = row?;
                upstreams.push((name, decode(&encoded)?));
            }

            Ok(upstreams)
        })
    }

    /// Create or replace an HTTP upstream.
    pub fn put_upstream(&self, name: &str, upstream: &Upstream) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(upstream).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.with(|connection| {
            connection.execute(
                "INSERT INTO upstreams (name, spec) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
                params![name, encoded],
            )?;
            Ok(())
        })
    }

    /// Remove an HTTP upstream.
    pub fn delete_upstream(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute("DELETE FROM upstreams WHERE name = ?1", [name])?;
            Ok(())
        })
    }

    /// Every overlay declared, by name (section 9.10).
    pub fn overlays(&self) -> Result<Vec<(String, Overlay)>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT name, spec FROM overlays ORDER BY name")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut overlays = Vec::new();

            for row in rows {
                let (name, encoded) = row?;
                overlays.push((name, decode(&encoded)?));
            }

            Ok(overlays)
        })
    }

    /// Create or replace an overlay's declaration.
    pub fn put_overlay(&self, name: &str, overlay: &Overlay) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(overlay).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.with(|connection| {
            connection.execute(
                "INSERT INTO overlays (name, spec) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
                params![name, encoded],
            )?;
            Ok(())
        })
    }

    /// Remove an overlay's declaration and its state.
    pub fn delete_overlay(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute("DELETE FROM overlays WHERE name = ?1", [name])?;
            connection.execute("DELETE FROM overlay_state WHERE name = ?1", [name])?;
            Ok(())
        })
    }

    /// Every overlay's state: active, or ended for a while.
    pub fn overlay_states(&self) -> Result<Vec<OverlayState>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT name, active, source, since_unix_ms, until_unix_ms \
                 FROM overlay_state ORDER BY name",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(OverlayState {
                    name: row.get(0)?,
                    active: row.get::<_, i64>(1)? != 0,
                    source: row.get(2)?,
                    since_unix_ms: row.get::<_, i64>(3)? as u64,
                    until_unix_ms: row.get::<_, Option<i64>>(4)?.map(|until| until as u64),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// Record an overlay's state, or clear it with `None`.
    pub fn set_overlay_state(
        &self,
        name: &str,
        state: Option<&OverlayState>,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            match state {
                Some(state) => {
                    connection.execute(
                        "INSERT INTO overlay_state (name, active, source, since_unix_ms, until_unix_ms) \
                         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(name) DO UPDATE SET \
                         active = excluded.active, source = excluded.source, \
                         since_unix_ms = excluded.since_unix_ms, until_unix_ms = excluded.until_unix_ms",
                        params![
                            name,
                            i64::from(state.active),
                            state.source,
                            state.since_unix_ms as i64,
                            state.until_unix_ms.map(|until| until as i64)
                        ],
                    )?;
                }
                None => {
                    connection.execute("DELETE FROM overlay_state WHERE name = ?1", [name])?;
                }
            }

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

    /// Create or replace a route, identified by its host and key: the
    /// path, and the client classes it is for (section 4.14).
    pub fn put_route(&self, route: &Route) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(route).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.with(|connection| {
            connection.execute(
                "INSERT INTO routes (host, path, spec) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(host, path) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
                params![route.host, route.key(), encoded],
            )?;
            Ok(())
        })
    }

    /// The rollouts of one application, or of all, oldest first (section
    /// 9.3).
    pub fn rollouts(&self, app: Option<&str>) -> Result<Vec<Rollout>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT spec FROM rollouts WHERE (?1 = '' OR app = ?1) ORDER BY app, created",
            )?;
            let rows = statement.query_map([app.unwrap_or("")], |row| row.get::<_, String>(0))?;
            let mut rollouts = Vec::new();

            for row in rows {
                rollouts.push(decode(&row?)?);
            }

            Ok(rollouts)
        })
    }

    /// One rollout by id, or the application's latest when `id` is empty.
    pub fn rollout(&self, app: &str, id: &str) -> Result<Rollout, StoreError> {
        self.with(|connection| {
            let encoded: Option<String> = connection
                .query_row(
                    "SELECT spec FROM rollouts WHERE app = ?1 AND (?2 = '' OR id = ?2) \
                     ORDER BY created DESC LIMIT 1",
                    params![app, id],
                    |row| row.get(0),
                )
                .optional()?;

            match encoded {
                Some(encoded) => decode(&encoded),
                None => Err(StoreError::NotFound(format!("rollout {app}/{id}"))),
            }
        })
    }

    /// The application's rollout that still shapes traffic, if any.
    pub fn active_rollout(&self, app: &str) -> Result<Option<Rollout>, StoreError> {
        Ok(self
            .rollouts(Some(app))?
            .into_iter()
            .find(|rollout| rollout.state.is_active()))
    }

    /// Create or replace a rollout record.
    pub fn put_rollout(&self, rollout: &Rollout) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(rollout).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.with(|connection| {
            connection.execute(
                "INSERT INTO rollouts (app, id, spec, state, created) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(app, id) DO UPDATE SET spec = excluded.spec, \
                 state = excluded.state, resource_version = resource_version + 1",
                params![
                    rollout.app,
                    rollout.id,
                    encoded,
                    rollout.state.as_str(),
                    rollout.created_unix_ms as i64
                ],
            )?;
            Ok(())
        })
    }

    /// Section 7.4: the worker counts an operator pinned on a release's
    /// groups, by group name.
    pub fn scale_pins(&self, app: &str, release: &str) -> Result<Vec<(String, u32)>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT grp, processes FROM scale_pins WHERE app = ?1 AND release = ?2 ORDER BY grp",
            )?;
            let rows = statement.query_map(params![app, release], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
            })?;
            let mut pins = Vec::new();

            for row in rows {
                pins.push(row?);
            }

            Ok(pins)
        })
    }

    /// Pin a group's worker count, replacing any earlier pin.
    pub fn put_scale_pin(
        &self,
        app: &str,
        release: &str,
        group: &str,
        processes: u32,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "INSERT INTO scale_pins (app, release, grp, processes) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(app, release, grp) DO UPDATE SET processes = excluded.processes",
                params![app, release, group, processes as i64],
            )?;
            Ok(())
        })
    }

    /// Remove a group's pin, or every pin of the release when no group is
    /// named.
    pub fn remove_scale_pins(
        &self,
        app: &str,
        release: &str,
        group: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "DELETE FROM scale_pins WHERE app = ?1 AND release = ?2 AND (?3 = '' OR grp = ?3)",
                params![app, release, group.unwrap_or("")],
            )?;
            Ok(())
        })
    }

    /// The next rollout id for an application: 1, 2, and so on.
    pub fn next_rollout_id(&self, app: &str) -> Result<String, StoreError> {
        self.with(|connection| {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM rollouts WHERE app = ?1",
                [app],
                |row| row.get(0),
            )?;
            Ok((count + 1).to_string())
        })
    }

    /// Remove a route, by its host and key.
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

    /// The sequence of the latest event, or 0 before any.
    pub fn last_event_sequence(&self) -> Result<u64, StoreError> {
        self.with(|connection| {
            let latest: Option<i64> =
                connection.query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))?;
            Ok(latest.unwrap_or(0) as u64)
        })
    }

    /// The sequence of the last event a sink delivered, or none for a
    /// sink the store has not seen.
    pub fn sink_cursor(&self, name: &str) -> Result<Option<u64>, StoreError> {
        self.with(|connection| {
            let delivered: Option<i64> = connection
                .query_row(
                    "SELECT delivered FROM event_sinks WHERE name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(delivered.map(|delivered| delivered as u64))
        })
    }

    /// Record the last event a sink delivered.
    pub fn set_sink_cursor(&self, name: &str, delivered: u64) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "INSERT INTO event_sinks (name, delivered) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET delivered = excluded.delivered",
                params![name, delivered as i64],
            )?;
            Ok(())
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
        user: row.get(5)?,
        group: row.get(6)?,
        umask: row.get(7)?,
    })
}

/// Add the identity columns of version 4 to an applications table that
/// predates them.
fn add_identity_columns(connection: &Connection) -> Result<(), StoreError> {
    let mut statement = connection.prepare("PRAGMA table_info(apps)")?;
    let columns: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;

    for column in ["user", "grp", "umask"] {
        if !columns.iter().any(|name| name == column) {
            connection.execute(&format!("ALTER TABLE apps ADD COLUMN {column} TEXT"), [])?;
        }
    }

    Ok(())
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
    use crate::model::ScalingSpec;
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
            reflection: false,
            groups: Vec::new(),
            mounts: Vec::new(),
            logging: Default::default(),
            max_requests: None,
            max_lifetime_ms: None,
            timeouts: Default::default(),
            memory: Default::default(),
            env: Vec::new(),
            scaling: ScalingSpec::default(),
            accel_redirect: false,
            cgi_local_redirect: false,
            sendfile_roots: Vec::new(),
            sendfile_header: None,
            error_pages: Default::default(),
            error_override: None,
            maintenance: None,
            http: None,
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
    fn sink_cursors_are_kept_by_name() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.sink_cursor("ops").unwrap(), None);
        assert_eq!(store.last_event_sequence().unwrap(), 0);
        let event = store
            .append_event("daemon.started", "daemon", "info", "up")
            .unwrap();
        assert_eq!(store.last_event_sequence().unwrap(), event.sequence);
        store.set_sink_cursor("ops", event.sequence).unwrap();
        store.set_sink_cursor("ops", event.sequence + 1).unwrap();
        assert_eq!(store.sink_cursor("ops").unwrap(), Some(event.sequence + 1));
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
