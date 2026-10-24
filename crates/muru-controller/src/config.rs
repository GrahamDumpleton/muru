//! Declarative configuration (design specification section 10.4): the
//! plan a document implies against desired state, which the API carries
//! out, and the export of desired state as a document that applies back
//! without changes.
//!
//! This slice covers applications and their releases. Listeners and
//! limits belong to the daemon's own file and are reported, not applied.

use std::path::{Path, PathBuf};

use muru_config::{App as AppTable, Document, Release as ReleaseTable};
use muru_proto::admin::v1 as pb;

use crate::model::{ReleaseSpec, valid_name};
use crate::store::{Store, StoreError};

/// What an apply does to one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Record a new application.
    CreateApp,
    /// Create a release with this spec and move traffic to it.
    Deploy(ReleaseSpec),
    /// Drain the release if it runs, then drop its record (prune).
    RemoveRelease,
    /// Drop an application whose releases are gone (prune).
    RemoveApp,
}

impl Action {
    /// The action's name as the report shows it.
    pub fn name(&self) -> &'static str {
        match self {
            Action::CreateApp => "create-app",
            Action::Deploy(_) => "deploy",
            Action::RemoveRelease => "remove-release",
            Action::RemoveApp => "remove-app",
        }
    }
}

/// One change of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// What happens.
    pub action: Action,
    /// The application.
    pub app: String,
    /// The release, for release actions.
    pub id: Option<String>,
    /// What the release runs, or why the object goes.
    pub detail: String,
}

impl Change {
    /// The subject as the API names it: app, or app@release.
    pub fn subject(&self) -> String {
        match &self.id {
            Some(id) => format!("{}@{id}", self.app),
            None => self.app.clone(),
        }
    }
}

/// What a document would change, or why it cannot be applied.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Validation failures; with any, nothing is applied.
    pub errors: Vec<String>,
    /// What the daemon takes no action on.
    pub warnings: Vec<String>,
    /// The changes, in the order they are carried out.
    pub changes: Vec<Change>,
    /// Releases already recorded as declared.
    pub unchanged: u32,
}

/// Compare a document with desired state. Creates and deploys come
/// first, application by application with releases in id order, so the
/// last declared release of an application is the one that serves; with
/// `prune`, removals of what the document does not contain follow.
pub fn plan(store: &Store, document: &Document, prune: bool) -> Plan {
    let mut plan = Plan::default();

    // Listeners and limits are node settings, read from the daemon's own
    // file at start-up (section 10.3).

    if !document.listener.is_empty() {
        plan.warnings.push(
            "[[listener]] entries belong in the daemon configuration file and were not applied"
                .to_owned(),
        );
    }
    if document.limits.is_some() {
        plan.warnings.push(
            "the [limits] table belongs in the daemon configuration file and was not applied"
                .to_owned(),
        );
    }

    for (name, app) in &document.app {
        plan_app(store, name, app, &mut plan);
    }

    if prune {
        plan_removals(store, document, &mut plan);
    }

    plan
}

/// The changes one application's table implies.
fn plan_app(store: &Store, name: &str, app: &AppTable, plan: &mut Plan) {
    if !valid_name(name) {
        plan.errors.push(format!(
            "{name:?} is not a valid application name: lowercase letters, digits and hyphens, up to 63 characters"
        ));
        return;
    }
    if store.app(name).is_err() {
        plan.changes.push(Change {
            action: Action::CreateApp,
            app: name.to_owned(),
            id: None,
            detail: String::new(),
        });
    }

    // Releases are resolved as a deploy would resolve them, from the
    // table and the source's own [tool.muru], never from an earlier
    // release, so a file means the same thing wherever it is applied.

    let mut deploys = Vec::new();
    for (id, table) in &app.release {
        if !valid_name(id) {
            plan.errors
                .push(format!("{name}@{id}: {id:?} is not a valid release id"));
            continue;
        }
        let Some(source) = &table.source else {
            plan.errors
                .push(format!("{name}@{id} has no source directory"));
            continue;
        };
        let spec = match resolve_spec(&request_from(name, table, source), None) {
            Ok(spec) => spec,
            Err(error) => {
                plan.errors.push(format!("{name}@{id}: {error}"));
                continue;
            }
        };

        match store.release(name, id) {
            Ok(recorded) if recorded.spec == spec => plan.unchanged += 1,
            Ok(recorded) => plan.errors.push(format!(
                "{name}@{id} already exists with different settings ({}); releases are immutable, so declare a new release id",
                differences(&spec, &recorded.spec).join(", ")
            )),
            Err(_) => deploys.push(Change {
                action: Action::Deploy(spec.clone()),
                app: name.to_owned(),
                id: Some(id.clone()),
                detail: describe(&spec),
            }),
        }
    }

    deploys.sort_by_key(|change| release_order(change.id.as_deref().unwrap_or_default()));
    plan.changes.extend(deploys);
}

/// With prune, releases and applications in desired state that the
/// document does not declare go.
fn plan_removals(store: &Store, document: &Document, plan: &mut Plan) {
    let Ok(apps) = store.apps() else {
        return;
    };

    for app in apps {
        let declared = document.app.get(&app.name);
        let Ok(releases) = store.releases(Some(&app.name)) else {
            continue;
        };

        for release in releases {
            let kept = declared.is_some_and(|table| table.release.contains_key(&release.id));
            if !kept {
                plan.changes.push(Change {
                    action: Action::RemoveRelease,
                    app: app.name.clone(),
                    id: Some(release.id.clone()),
                    detail: format!("{} and not declared", release.state),
                });
            }
        }
        if declared.is_none() {
            plan.changes.push(Change {
                action: Action::RemoveApp,
                app: app.name.clone(),
                id: None,
                detail: "not declared".to_owned(),
            });
        }
    }
}

/// Desired state as a document: each application with the release that
/// serves it, or with every release. Applying it back changes nothing.
pub fn export(
    store: &Store,
    app: Option<&str>,
    all_releases: bool,
) -> Result<Document, StoreError> {
    let apps = match app {
        Some(name) => vec![store.app(name)?],
        None => store.apps()?,
    };
    let mut document = Document::default();

    for app in apps {
        let mut releases = store.releases(Some(&app.name))?;
        releases.sort_by_key(|release| release_order(&release.id));
        let mut table = AppTable::default();

        for release in releases {
            let serves = app.current.as_deref() == Some(release.id.as_str());
            if all_releases || serves {
                table.release.insert(release.id, table_from(&release.spec));
            }
        }
        document.app.insert(app.name, table);
    }

    Ok(document)
}

/// Resolve a deploy request into a release spec: the request's settings,
/// then the source's [tool.muru] table, then the previous release when
/// one is given (a deploy), then the defaults of section 13.1.
pub fn resolve_spec(
    request: &pb::DeployRequest,
    previous: Option<&ReleaseSpec>,
) -> Result<ReleaseSpec, String> {
    let source = Path::new(&request.source);
    if !source.is_dir() {
        return Err(format!("{} is not a directory", source.display()));
    }
    let source = source
        .canonicalize()
        .map_err(|error| format!("{}: {error}", request.source))?;
    let table = muru_config::load_project(&source)
        .map_err(|error| error.to_string())?
        .map(|(_, table)| table)
        .unwrap_or_default();

    // The entrypoint from the request replaces the table's with its
    // factory settings, as muructl run does (section 13.1).

    let (entrypoint, from_table) = if !request.entrypoint.is_empty() {
        (request.entrypoint.clone(), false)
    } else if let Some(entrypoint) = table.entrypoint.clone() {
        (entrypoint, true)
    } else if let Some(previous) = previous {
        (previous.entrypoint.clone(), false)
    } else {
        return Err(format!(
            "{} has no entrypoint in its [tool.muru] table; give one as module:callable",
            source.display()
        ));
    };
    let factory = match (request.factory, from_table) {
        (Some(true), _) => Some(parse_or(
            &request.factory_phase,
            muru_config::FactoryPhase::Worker,
        )?),
        (Some(false), _) => None,
        (None, true) if table.factory == Some(true) => Some(
            table
                .factory_phase
                .unwrap_or(muru_config::FactoryPhase::Worker),
        ),
        (None, _) => None,
    };

    // The interpreter: as given, else the source's own virtualenv, else
    // what the previous release ran, else the python3 on the path.

    let python = if !request.python.is_empty() {
        PathBuf::from(&request.python)
    } else {
        let venv = source.join(".venv").join("bin").join("python");
        if venv.is_file() {
            venv
        } else if let Some(previous) = previous {
            previous.python.clone()
        } else {
            PathBuf::from("python3")
        }
    };

    Ok(ReleaseSpec {
        source,
        entrypoint,
        interface: match request.interface.as_str() {
            "" => table.interface.unwrap_or(muru_config::Interface::Auto),
            given => given
                .parse()
                .map_err(|error: muru_config::ParseError| error.to_string())?,
        },
        python,
        processes: request
            .processes
            .or(table.processes.map(|value| value as u32))
            .or(previous.map(|previous| previous.processes))
            .unwrap_or(1),
        threads: request
            .threads
            .or(table.threads.map(|value| value as u32))
            .or(previous.map(|previous| previous.threads))
            .unwrap_or(4),
        sync_threads: request
            .sync_threads
            .or(table.sync_threads.map(|value| value as u32))
            .unwrap_or(8),
        concurrency: request
            .concurrency
            .or(table.concurrency.map(|value| value as u32))
            .unwrap_or(1000),
        preload: match request.preload.as_str() {
            "" => table.preload.unwrap_or(muru_config::Preload::Auto),
            given => given
                .parse()
                .map_err(|error: muru_config::ParseError| error.to_string())?,
        },
        hooks: (!request.hooks.is_empty())
            .then(|| request.hooks.clone())
            .or(table.hooks),
        factory,
    })
}

/// The host name, for export headers.
pub fn hostname() -> String {
    let mut buffer = [0u8; 256];

    // SAFETY: gethostname writes at most `len` bytes into a buffer this
    // frame owns, and returns non-zero rather than overrunning it.

    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return String::new();
    }

    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

fn parse_or<T: std::str::FromStr<Err = muru_config::ParseError>>(
    text: &str,
    default: T,
) -> Result<T, String> {
    if text.is_empty() {
        return Ok(default);
    }

    text.parse()
        .map_err(|error: muru_config::ParseError| error.to_string())
}

/// A declared release as the deploy request it stands for.
fn request_from(app: &str, table: &ReleaseTable, source: &str) -> pb::DeployRequest {
    pb::DeployRequest {
        app: app.to_owned(),
        source: source.to_owned(),
        entrypoint: table.entrypoint.clone(),
        interface: table
            .interface
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        python: table.python.clone().unwrap_or_default(),
        processes: table.processes.map(|value| value as u32),
        threads: table.threads.map(|value| value as u32),
        sync_threads: table.sync_threads.map(|value| value as u32),
        concurrency: table.concurrency.map(|value| value as u32),
        preload: table
            .preload
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        hooks: table.hooks.clone().unwrap_or_default(),
        factory: table.factory,
        factory_phase: table
            .factory_phase
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        release: String::new(),
        wait: true,
        retain_ms: None,
    }
}

/// A recorded spec as the table that declares it, every key written so
/// the file means the same thing without the source's [tool.muru].
fn table_from(spec: &ReleaseSpec) -> ReleaseTable {
    ReleaseTable {
        source: Some(spec.source.display().to_string()),
        entrypoint: spec.entrypoint.clone(),
        interface: Some(spec.interface),
        factory: spec.factory.map(|_| true),
        factory_phase: spec.factory,
        python: Some(spec.python.display().to_string()),
        processes: Some(spec.processes as usize),
        threads: Some(spec.threads as usize),
        sync_threads: Some(spec.sync_threads as usize),
        concurrency: Some(spec.concurrency as usize),
        preload: Some(spec.preload),
        hooks: spec.hooks.clone(),
    }
}

/// What a release runs, for the report: entrypoint, processes by threads
/// and the source.
fn describe(spec: &ReleaseSpec) -> String {
    format!(
        "{} {}x{} from {}",
        spec.entrypoint,
        spec.processes,
        spec.threads,
        spec.source.display()
    )
}

/// The settings on which a declared spec differs from a recorded one.
fn differences(declared: &ReleaseSpec, recorded: &ReleaseSpec) -> Vec<String> {
    let mut found = Vec::new();
    let mut compare = |name: &str, declared: String, recorded: String| {
        if declared != recorded {
            found.push(format!("{name} {declared}, recorded {recorded}"));
        }
    };

    compare(
        "source",
        declared.source.display().to_string(),
        recorded.source.display().to_string(),
    );
    compare(
        "entrypoint",
        declared.entrypoint.clone(),
        recorded.entrypoint.clone(),
    );
    compare(
        "interface",
        declared.interface.to_string(),
        recorded.interface.to_string(),
    );
    compare(
        "python",
        declared.python.display().to_string(),
        recorded.python.display().to_string(),
    );
    compare(
        "processes",
        declared.processes.to_string(),
        recorded.processes.to_string(),
    );
    compare(
        "threads",
        declared.threads.to_string(),
        recorded.threads.to_string(),
    );
    compare(
        "sync_threads",
        declared.sync_threads.to_string(),
        recorded.sync_threads.to_string(),
    );
    compare(
        "concurrency",
        declared.concurrency.to_string(),
        recorded.concurrency.to_string(),
    );
    compare(
        "preload",
        declared.preload.to_string(),
        recorded.preload.to_string(),
    );
    compare(
        "hooks",
        declared.hooks.clone().unwrap_or_default(),
        recorded.hooks.clone().unwrap_or_default(),
    );
    compare(
        "factory",
        declared
            .factory
            .map(|phase| phase.to_string())
            .unwrap_or_default(),
        recorded
            .factory
            .map(|phase| phase.to_string())
            .unwrap_or_default(),
    );

    found
}

/// Generated ids in numeric order (r2 before r10), then any other id.
fn release_order(id: &str) -> (u8, u64, String) {
    match id
        .strip_prefix('r')
        .and_then(|rest| rest.parse::<u64>().ok())
    {
        Some(number) => (0, number, String::new()),
        None => (1, 0, id.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReleaseState;

    fn store_with_shop(source: &Path) -> Store {
        let store = Store::in_memory().unwrap();
        store.ensure_app("shop").unwrap();
        let spec = resolve_spec(
            &pb::DeployRequest {
                app: "shop".to_owned(),
                source: source.display().to_string(),
                entrypoint: "shop.wsgi:application".to_owned(),
                python: "/usr/bin/python3".to_owned(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        store.create_release("shop", "r1", &spec).unwrap();
        store
            .set_release_state("shop", "r1", ReleaseState::Ready, "")
            .unwrap();
        store.set_app_releases("shop", Some("r1"), None).unwrap();

        store
    }

    fn source_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muru-config-plan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_export_applies_back_without_changes() {
        let source = source_dir();
        let store = store_with_shop(&source);
        let document = export(&store, None, false).unwrap();
        let text = document.to_toml().unwrap();
        assert!(text.contains("[app.shop.release.r1]"), "{text}");

        let plan = plan(&store, &Document::parse(&text).unwrap(), true);
        assert!(plan.errors.is_empty(), "{:?}", plan.errors);
        assert!(plan.changes.is_empty(), "{:?}", plan.changes);
        assert_eq!(plan.unchanged, 1);
    }

    #[test]
    fn an_application_without_a_serving_release_still_exports() {
        let source = source_dir();
        let store = store_with_shop(&source);
        store.set_app_releases("shop", None, Some("r1")).unwrap();

        let text = export(&store, None, false).unwrap().to_toml().unwrap();
        let back = Document::parse(&text).unwrap();
        assert!(back.app.contains_key("shop"), "{text}");
        assert!(back.app["shop"].release.is_empty(), "{text}");
    }

    #[test]
    fn a_new_release_is_deployed_and_a_changed_one_refused() {
        let source = source_dir();
        let store = store_with_shop(&source);
        let text = format!(
            "[app.shop.release.r1]\nsource = '{0}'\nentrypoint = 'shop.wsgi:application'\npython = '/usr/bin/python3'\nthreads = 2\n\
             [app.shop.release.r2]\nsource = '{0}'\nentrypoint = 'shop.wsgi:application'\npython = '/usr/bin/python3'\n\
             [app.blog.release.r1]\nsource = '{0}'\nentrypoint = 'blog:app'\npython = '/usr/bin/python3'\n",
            source.display()
        );

        let plan = plan(&store, &Document::parse(&text).unwrap(), false);
        assert_eq!(plan.errors.len(), 1, "{:?}", plan.errors);
        assert!(
            plan.errors[0].contains("shop@r1 already exists")
                && plan.errors[0].contains("threads 2, recorded 4"),
            "{}",
            plan.errors[0]
        );
        let names: Vec<(&str, String)> = plan
            .changes
            .iter()
            .map(|change| (change.action.name(), change.subject()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("create-app", "blog".to_owned()),
                ("deploy", "blog@r1".to_owned()),
                ("deploy", "shop@r2".to_owned()),
            ]
        );
    }

    #[test]
    fn prune_removes_what_is_not_declared() {
        let source = source_dir();
        let store = store_with_shop(&source);
        let plan = plan(&store, &Document::default(), true);
        let names: Vec<(&str, String)> = plan
            .changes
            .iter()
            .map(|change| (change.action.name(), change.subject()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("remove-release", "shop@r1".to_owned()),
                ("remove-app", "shop".to_owned()),
            ]
        );
        assert!(plan.errors.is_empty());
    }

    #[test]
    fn validation_names_every_problem() {
        let store = Store::in_memory().unwrap();
        let text = "[app.Shop.release.r1]\nentrypoint = 'a:b'\n\
                    [app.shop.release.r1]\nentrypoint = 'a:b'\n\
                    [app.shop.release.r2]\nsource = '/nonexistent/dir'\n\
                    [[listener]]\naddress = '127.0.0.1:8080'\n";

        let plan = plan(&store, &Document::parse(text).unwrap(), false);
        assert_eq!(plan.errors.len(), 3, "{:?}", plan.errors);
        assert!(plan.errors[0].contains("\"Shop\" is not a valid application name"));
        assert!(plan.errors[1].contains("shop@r1 has no source directory"));
        assert!(plan.errors[2].contains("is not a directory"));
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.changes.len(), 1, "the app is still created");
    }

    #[test]
    fn generated_ids_sort_numerically() {
        let mut ids = vec!["r10", "r2", "blue", "r1"];
        ids.sort_by_key(|id| release_order(id));
        assert_eq!(ids, vec!["r1", "r2", "r10", "blue"]);
    }
}
