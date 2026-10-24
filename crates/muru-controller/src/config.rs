//! Declarative configuration (design specification section 10.4): the
//! plan a document implies against desired state, which the API carries
//! out, and the export of desired state as a document that applies back
//! without changes.
//!
//! This covers applications, releases, virtual hosts and routes.
//! Listeners and limits belong to the daemon's own file and are reported,
//! not applied.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use muru_config::{
    App as AppTable, Document, Group as GroupTable, Host as VirtualHost, Mount as MountTable,
    Release as ReleaseTable, Route as RouteTable,
};
use muru_proto::admin::v1 as pb;

use crate::model::{GroupSpec, MountSpec, ReleaseSpec, valid_name};
use crate::store::{Store, StoreError};

/// What an apply does to one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Record a new application.
    CreateApp {
        /// The application.
        app: String,
    },
    /// Create a release with this spec and move traffic to it.
    Deploy {
        /// The application.
        app: String,
        /// The declared release id.
        id: String,
        /// The resolved spec.
        spec: ReleaseSpec,
    },
    /// Drain the release if it runs, then drop its record (prune).
    RemoveRelease {
        /// The application.
        app: String,
        /// The release.
        id: String,
    },
    /// Drop an application whose releases are gone (prune).
    RemoveApp {
        /// The application.
        app: String,
    },
    /// Create or replace a virtual host.
    PutHost {
        /// The host's name.
        name: String,
        /// Its definition.
        host: VirtualHost,
        /// Whether it is new rather than changed.
        new: bool,
    },
    /// Drop a virtual host and its routes (prune).
    RemoveHost {
        /// The host's name.
        name: String,
    },
    /// Create or replace a route.
    PutRoute {
        /// The route.
        route: RouteTable,
        /// Whether it is new rather than changed.
        new: bool,
    },
    /// Drop a route (prune).
    RemoveRoute {
        /// The host.
        host: String,
        /// The path prefix.
        path: String,
    },
}

impl Action {
    /// The action's name as the report shows it.
    pub fn name(&self) -> &'static str {
        match self {
            Action::CreateApp { .. } => "create-app",
            Action::Deploy { .. } => "deploy",
            Action::RemoveRelease { .. } => "remove-release",
            Action::RemoveApp { .. } => "remove-app",
            Action::PutHost { new: true, .. } => "create-host",
            Action::PutHost { new: false, .. } => "update-host",
            Action::RemoveHost { .. } => "remove-host",
            Action::PutRoute { new: true, .. } => "create-route",
            Action::PutRoute { new: false, .. } => "update-route",
            Action::RemoveRoute { .. } => "remove-route",
        }
    }

    /// Whether the edge's snapshot changes with the action.
    pub fn reconfigures(&self) -> bool {
        matches!(
            self,
            Action::PutHost { .. }
                | Action::RemoveHost { .. }
                | Action::PutRoute { .. }
                | Action::RemoveRoute { .. }
        )
    }
}

/// One change of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// What happens.
    pub action: Action,
    /// What the release runs, or why the object goes.
    pub detail: String,
}

impl Change {
    /// The subject as the API names it: app, app@release, host NAME, or
    /// route HOST PATH.
    pub fn subject(&self) -> String {
        match &self.action {
            Action::CreateApp { app } | Action::RemoveApp { app } => app.clone(),
            Action::Deploy { app, id, .. } | Action::RemoveRelease { app, id } => {
                format!("{app}@{id}")
            }
            Action::PutHost { name, .. } | Action::RemoveHost { name } => format!("host {name}"),
            Action::PutRoute { route, .. } => format!("route {} {}", route.host, route.path),
            Action::RemoveRoute { host, path } => format!("route {host} {path}"),
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
    /// Releases, hosts and routes already recorded as declared.
    pub unchanged: u32,
}

/// Compare a document with desired state. Creates and deploys come
/// first, application by application with releases in id order, so the
/// last declared release of an application is the one that serves; hosts
/// and routes follow; with `prune`, removals of what the document does
/// not contain come last.
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

    if document.proxy.is_some() {
        plan.warnings.push(
            "the [proxy] table belongs in the daemon configuration file and was not applied"
                .to_owned(),
        );
    }

    if document.metrics.is_some() {
        plan.warnings.push(
            "the [metrics] table belongs in the daemon configuration file and was not applied"
                .to_owned(),
        );
    }

    if document.events.is_some() {
        plan.warnings.push(
            "the [events] table belongs in the daemon configuration file and was not applied"
                .to_owned(),
        );
    }

    for (name, app) in &document.app {
        plan_app(store, name, app, &mut plan);
    }

    plan_hosts(store, document, &mut plan);
    plan_routes(store, document, &mut plan);

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
            action: Action::CreateApp {
                app: name.to_owned(),
            },
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

        let groups = (!table.groups.is_empty()).then_some(&table.groups);
        let mounts = (!table.mounts.is_empty()).then_some(&table.mounts);

        let spec = match resolve_spec(
            &request_from(name, table, source),
            None,
            groups,
            mounts,
            table.logging.as_ref(),
        ) {
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
                detail: describe(&spec),
                action: Action::Deploy {
                    app: name.to_owned(),
                    id: id.clone(),
                    spec,
                },
            }),
        }
    }

    deploys.sort_by_key(|change| match &change.action {
        Action::Deploy { id, .. } => release_order(id),
        _ => release_order(""),
    });
    plan.changes.extend(deploys);
}

/// The changes the document's hosts imply (section 4.3): names are
/// checked, duplicates refused and certificates read, so a bad host
/// never reaches the edge.
fn plan_hosts(store: &Store, document: &Document, plan: &mut Plan) {
    let stored: BTreeMap<String, VirtualHost> =
        store.hosts().unwrap_or_default().into_iter().collect();
    let mut seen_names: BTreeMap<String, String> = BTreeMap::new();
    let mut defaults = 0;

    for (name, host) in &document.host {
        if !valid_name(name) {
            plan.errors
                .push(format!("host {name:?} is not a valid name"));
            continue;
        }

        let mut valid = true;

        for declared in &host.names {
            let normalised = muru_edge::routing::normalise(declared);
            let label = normalised.strip_prefix("*.").unwrap_or(&normalised);

            if label.is_empty() || label.contains('*') || label.contains('/') {
                plan.errors
                    .push(format!("host {name}: {declared:?} is not a host name"));
                valid = false;
                continue;
            }

            if let Some(other) = seen_names.insert(normalised.clone(), name.clone())
                && other != *name
            {
                plan.errors.push(format!(
                    "host {name}: {normalised} is also a name of host {other}"
                ));
                valid = false;
            }
        }

        if host.names.is_empty() && !host.default {
            plan.errors
                .push(format!("host {name} has no names and is not the default"));
            valid = false;
        }

        if host.use_canonical_name && host.canonical.is_none() {
            plan.errors.push(format!(
                "host {name}: use_canonical_name needs a canonical name"
            ));
            valid = false;
        }

        if host.default {
            defaults += 1;
        }

        if let Some(tls) = &host.tls {
            let loaded =
                muru_edge::tls::load_pem_files(Path::new(&tls.certificate), Path::new(&tls.key))
                    .and_then(|(chain, key)| muru_edge::tls::certified_key(chain, key));

            if let Err(error) = loaded {
                plan.errors.push(format!("host {name}: {error:#}"));
                valid = false;
            }
        }

        if !valid {
            continue;
        }

        match stored.get(name) {
            Some(recorded) if recorded == host => plan.unchanged += 1,
            recorded => plan.changes.push(Change {
                action: Action::PutHost {
                    name: name.clone(),
                    host: host.clone(),
                    new: recorded.is_none(),
                },
                detail: describe_host(host),
            }),
        }
    }

    if defaults > 1 {
        plan.errors
            .push("more than one host is the default".to_owned());
    }
}

/// The changes the document's routes imply (section 4.5).
fn plan_routes(store: &Store, document: &Document, plan: &mut Plan) {
    let stored: BTreeMap<(String, String), RouteTable> = store
        .routes()
        .unwrap_or_default()
        .into_iter()
        .map(|route| ((route.host.clone(), route.path.clone()), route))
        .collect();
    let stored_hosts: Vec<String> = store
        .hosts()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let stored_apps: Vec<String> = store
        .apps()
        .unwrap_or_default()
        .into_iter()
        .map(|app| app.name)
        .collect();
    let mut seen = Vec::new();

    for route in &document.route {
        let key = (route.host.clone(), route.path.clone());
        let mut valid = true;

        if !route.path.starts_with('/') {
            plan.errors.push(format!(
                "route {} {}: the path must start with /",
                route.host, route.path
            ));
            valid = false;
        }

        if !document.host.contains_key(&route.host) && !stored_hosts.contains(&route.host) {
            plan.errors.push(format!(
                "route {} {}: no host named {}",
                route.host, route.path, route.host
            ));
            valid = false;
        }

        if !document.app.contains_key(&route.app) && !stored_apps.contains(&route.app) {
            plan.errors.push(format!(
                "route {} {}: no application named {}",
                route.host, route.path, route.app
            ));
            valid = false;
        }

        if seen.contains(&key) {
            plan.errors.push(format!(
                "route {} {} is declared twice",
                route.host, route.path
            ));
            valid = false;
        }

        seen.push(key.clone());

        if !valid {
            continue;
        }

        match stored.get(&key) {
            Some(recorded) if recorded == route => plan.unchanged += 1,
            recorded => plan.changes.push(Change {
                action: Action::PutRoute {
                    route: route.clone(),
                    new: recorded.is_none(),
                },
                detail: format!("to {}", route.app),
            }),
        }
    }
}

/// With prune, routes, hosts, releases and applications in desired state
/// that the document does not declare go, in that order.
fn plan_removals(store: &Store, document: &Document, plan: &mut Plan) {
    for route in store.routes().unwrap_or_default() {
        let declared = document
            .route
            .iter()
            .any(|candidate| candidate.host == route.host && candidate.path == route.path);

        if !declared {
            plan.changes.push(Change {
                action: Action::RemoveRoute {
                    host: route.host,
                    path: route.path,
                },
                detail: "not declared".to_owned(),
            });
        }
    }

    for (name, _) in store.hosts().unwrap_or_default() {
        if !document.host.contains_key(&name) {
            plan.changes.push(Change {
                action: Action::RemoveHost { name },
                detail: "not declared".to_owned(),
            });
        }
    }

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
                    action: Action::RemoveRelease {
                        app: app.name.clone(),
                        id: release.id.clone(),
                    },
                    detail: format!("{} and not declared", release.state),
                });
            }
        }

        if declared.is_none() {
            plan.changes.push(Change {
                action: Action::RemoveApp {
                    app: app.name.clone(),
                },
                detail: "not declared".to_owned(),
            });
        }
    }
}

/// Desired state as a document: each application with the release that
/// serves it, or with every release, and the hosts and routes. Applying
/// it back changes nothing.
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

    // Hosts and routes are node-wide; with one application asked for,
    // only the routes to it and their hosts are written.

    let routes = store.routes()?;
    let hosts = store.hosts()?;

    for route in routes {
        if app.is_none_or(|name| route.app == name) {
            document.route.push(route);
        }
    }

    for (name, host) in hosts {
        let routed = document.route.iter().any(|route| route.host == name);

        if app.is_none() || routed {
            document.host.insert(name, host);
        }
    }

    Ok(document)
}

/// A host for the report: its names, and whether it has TLS.
fn describe_host(host: &VirtualHost) -> String {
    let mut parts = vec![if host.names.is_empty() {
        "no names".to_owned()
    } else {
        host.names.join(", ")
    }];

    if host.tls.is_some() {
        parts.push("with TLS".to_owned());
    }

    if host.default {
        parts.push("default".to_owned());
    }

    parts.join(", ")
}

/// Resolve a deploy request into a release spec: the request's settings,
/// then the source's [tool.muru] table, then the previous release when
/// one is given (a deploy), then the defaults of section 13.1.
pub fn resolve_spec(
    request: &pb::DeployRequest,
    previous: Option<&ReleaseSpec>,
    groups: Option<&BTreeMap<String, GroupTable>>,
    mounts: Option<&BTreeMap<String, MountTable>>,
    logging: Option<&muru_config::Logging>,
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

    // Section 7.1: groups from the declaration, else the source's own
    // table; each takes its sizes from the release's where it is silent.

    let processes = request
        .processes
        .or(table.processes.map(|value| value as u32))
        .or(previous.map(|previous| previous.processes))
        .unwrap_or(1);
    let threads = request
        .threads
        .or(table.threads.map(|value| value as u32))
        .or(previous.map(|previous| previous.threads))
        .unwrap_or(4);
    let sync_threads = request
        .sync_threads
        .or(table.sync_threads.map(|value| value as u32))
        .unwrap_or(8);
    let concurrency = request
        .concurrency
        .or(table.concurrency.map(|value| value as u32))
        .unwrap_or(1000);
    let mut resolved_groups = Vec::new();

    for (name, group) in groups.unwrap_or(&table.groups) {
        if !crate::model::valid_name(name) || name == "default" {
            return Err(format!("{name:?} is not a valid worker group name"));
        }

        if group.paths.is_empty() {
            return Err(format!("worker group {name} has no paths"));
        }

        if let Some(path) = group.paths.iter().find(|path| !path.starts_with('/')) {
            return Err(format!(
                "worker group {name}: {path:?} does not start with /"
            ));
        }

        if group.processes == Some(0) {
            return Err(format!("worker group {name} has no processes"));
        }

        resolved_groups.push(GroupSpec {
            name: name.clone(),
            paths: group.paths.clone(),
            processes: group.processes.map_or(processes, |value| value as u32),
            threads: group.threads.map_or(threads, |value| value as u32),
            sync_threads: group
                .sync_threads
                .map_or(sync_threads, |value| value as u32),
            concurrency: group.concurrency.map_or(concurrency, |value| value as u32),
        });
    }

    // Section 6.7: mounts from the declaration, else the source's own
    // table; each has a name, a path below / and an entrypoint, and no
    // two share a path.

    let mut resolved_mounts: Vec<MountSpec> = Vec::new();

    for (name, mount) in mounts.unwrap_or(&table.mounts) {
        if !crate::model::valid_name(name) {
            return Err(format!("{name:?} is not a valid mount name"));
        }

        if !mount.path.starts_with('/') || mount.path == "/" {
            return Err(format!(
                "mount {name}: {:?} is not a path below /",
                mount.path
            ));
        }

        if mount.entrypoint.is_empty() || !mount.entrypoint.contains(':') {
            return Err(format!(
                "mount {name}: the entrypoint must be module:callable"
            ));
        }

        let path = mount.path.trim_end_matches('/').to_owned();

        if resolved_mounts.iter().any(|other| other.path == path) {
            return Err(format!("mount {name}: another mount has the path {path}"));
        }

        resolved_mounts.push(MountSpec {
            name: name.clone(),
            path,
            entrypoint: mount.entrypoint.clone(),
            interface: mount.interface.unwrap_or(muru_config::Interface::Auto),
        });
    }

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
        processes,
        threads,
        sync_threads,
        concurrency,
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
        groups: resolved_groups,
        mounts: resolved_mounts,
        logging: logging
            .or(table.logging.as_ref())
            .and_then(|logging| logging.python.clone())
            .unwrap_or_default(),
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
        groups: spec
            .groups
            .iter()
            .map(|group| {
                (
                    group.name.clone(),
                    GroupTable {
                        paths: group.paths.clone(),
                        processes: Some(group.processes as usize),
                        threads: Some(group.threads as usize),
                        sync_threads: Some(group.sync_threads as usize),
                        concurrency: Some(group.concurrency as usize),
                    },
                )
            })
            .collect(),
        mounts: spec
            .mounts
            .iter()
            .map(|mount| {
                (
                    mount.name.clone(),
                    MountTable {
                        path: mount.path.clone(),
                        entrypoint: mount.entrypoint.clone(),
                        interface: Some(mount.interface),
                    },
                )
            })
            .collect(),
        logging: (spec.logging != muru_config::PythonLogging::default()).then(|| {
            muru_config::Logging {
                python: Some(spec.logging.clone()),
            }
        }),
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
    compare(
        "groups",
        describe_groups(&declared.groups),
        describe_groups(&recorded.groups),
    );
    compare(
        "mounts",
        describe_mounts(&declared.mounts),
        describe_mounts(&recorded.mounts),
    );

    found
}

/// Groups as one phrase, for the report.
fn describe_groups(groups: &[GroupSpec]) -> String {
    if groups.is_empty() {
        return "none".to_owned();
    }

    groups
        .iter()
        .map(|group| {
            format!(
                "{} {}x{} on {}",
                group.name,
                group.processes,
                group.threads,
                group.paths.join(" ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Mounts as one phrase, for the report.
fn describe_mounts(mounts: &[MountSpec]) -> String {
    if mounts.is_empty() {
        return "none".to_owned();
    }

    mounts
        .iter()
        .map(|mount| format!("{} {} at {}", mount.name, mount.entrypoint, mount.path))
        .collect::<Vec<_>>()
        .join("; ")
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
            None,
            None,
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
