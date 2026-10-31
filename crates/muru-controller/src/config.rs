//! Declarative configuration (design specification section 10.4): the
//! plan a document implies against desired state, which the API carries
//! out, and the export of desired state as a document that applies back
//! without changes.
//!
//! This covers applications, releases, virtual hosts, routes and HTTP
//! upstreams.
//! Listeners and limits belong to the daemon's own file and are reported,
//! not applied.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use muru_config::{
    App as AppTable, ClientClass, Document, Group as GroupTable, Host as VirtualHost,
    Mount as MountTable, Overlay, RateLimit, Release as ReleaseTable, Route as RouteTable,
    ScalingMode, Upstream as UpstreamTable,
};
use muru_proto::admin::v1 as pb;

use crate::model::{
    EnvSpec, GroupSpec, HttpServerSpec, MountSpec, ReleaseSpec, ScalingSpec, StaticSpec, valid_name,
};
use crate::store::{Store, StoreError};

/// What an apply does to one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Record a new application.
    CreateApp {
        /// The application.
        app: String,
    },
    /// Set the user, group and umask an application's processes run
    /// with (section 10.9).
    SetAppIdentity {
        /// The application.
        app: String,
        /// The user, or none for the daemon's.
        user: Option<String>,
        /// The group, or none for the user's primary group.
        group: Option<String>,
        /// The umask, in octal, or none for the daemon's.
        umask: Option<String>,
    },
    /// Set the priority by which the node's budget admits the
    /// application's scale-ups (section 7.4).
    SetAppPriority {
        /// The application.
        app: String,
        /// The priority, 0 by default.
        priority: i32,
    },
    /// Create a release with this spec and move traffic to it.
    Deploy {
        /// The application.
        app: String,
        /// The declared release id.
        id: String,
        /// The resolved spec, boxed to keep the actions small.
        spec: Box<ReleaseSpec>,
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
        route: Box<RouteTable>,
        /// Whether it is new rather than changed.
        new: bool,
    },
    /// Drop a route (prune).
    RemoveRoute {
        /// The host.
        host: String,
        /// The route's key: its path prefix, and the client classes it
        /// is for.
        path: String,
    },
    /// Create or replace an HTTP upstream (section 4.13).
    PutUpstream {
        /// The upstream's name.
        name: String,
        /// Its definition.
        upstream: Box<UpstreamTable>,
        /// Whether it is new rather than changed.
        new: bool,
    },
    /// Drop an HTTP upstream (prune).
    RemoveUpstream {
        /// The upstream's name.
        name: String,
    },
    /// Create or replace an overlay (section 9.10).
    PutOverlay {
        /// The overlay's name.
        name: String,
        /// Its declaration.
        overlay: Box<Overlay>,
        /// Whether it is new rather than changed.
        new: bool,
    },
    /// Drop an overlay, ending it if it is active (prune).
    RemoveOverlay {
        /// The overlay's name.
        name: String,
    },
    /// Create or replace a client class (section 4.14).
    PutClientClass {
        /// Its rule.
        class: Box<ClientClass>,
        /// Where its rule is tried among the applied ones.
        position: i64,
        /// Whether it is new rather than changed.
        new: bool,
    },
    /// Drop a client class (prune).
    RemoveClientClass {
        /// The class's name.
        name: String,
    },
    /// Create or replace a rate limit (section 4.15).
    PutRateLimit {
        /// The limit's name.
        name: String,
        /// Its table.
        limit: Box<RateLimit>,
        /// Whether it is new rather than changed.
        new: bool,
    },
    /// Drop a rate limit (prune).
    RemoveRateLimit {
        /// The limit's name.
        name: String,
    },
}

impl Action {
    /// The action's name as the report shows it.
    pub fn name(&self) -> &'static str {
        match self {
            Action::CreateApp { .. } => "create-app",
            Action::SetAppIdentity { .. } => "set-identity",
            Action::SetAppPriority { .. } => "set-priority",
            Action::Deploy { .. } => "deploy",
            Action::RemoveRelease { .. } => "remove-release",
            Action::RemoveApp { .. } => "remove-app",
            Action::PutHost { new: true, .. } => "create-host",
            Action::PutHost { new: false, .. } => "update-host",
            Action::RemoveHost { .. } => "remove-host",
            Action::PutRoute { new: true, .. } => "create-route",
            Action::PutRoute { new: false, .. } => "update-route",
            Action::RemoveRoute { .. } => "remove-route",
            Action::PutUpstream { new: true, .. } => "create-upstream",
            Action::PutUpstream { new: false, .. } => "update-upstream",
            Action::RemoveUpstream { .. } => "remove-upstream",
            Action::PutOverlay { new: true, .. } => "create-overlay",
            Action::PutOverlay { new: false, .. } => "update-overlay",
            Action::RemoveOverlay { .. } => "remove-overlay",
            Action::PutClientClass { new: true, .. } => "create-client-class",
            Action::PutClientClass { new: false, .. } => "update-client-class",
            Action::RemoveClientClass { .. } => "remove-client-class",
            Action::PutRateLimit { new: true, .. } => "create-rate-limit",
            Action::PutRateLimit { new: false, .. } => "update-rate-limit",
            Action::RemoveRateLimit { .. } => "remove-rate-limit",
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
                | Action::PutUpstream { .. }
                | Action::RemoveUpstream { .. }
                | Action::PutOverlay { .. }
                | Action::RemoveOverlay { .. }
                | Action::PutClientClass { .. }
                | Action::RemoveClientClass { .. }
                | Action::PutRateLimit { .. }
                | Action::RemoveRateLimit { .. }
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
            Action::CreateApp { app }
            | Action::RemoveApp { app }
            | Action::SetAppIdentity { app, .. }
            | Action::SetAppPriority { app, .. } => app.clone(),
            Action::Deploy { app, id, .. } | Action::RemoveRelease { app, id } => {
                format!("{app}@{id}")
            }
            Action::PutHost { name, .. } | Action::RemoveHost { name } => format!("host {name}"),
            Action::PutRoute { route, .. } => format!("route {} {}", route.host, route.key()),
            Action::RemoveRoute { host, path } => format!("route {host} {path}"),
            Action::PutUpstream { name, .. } | Action::RemoveUpstream { name } => {
                format!("upstream {name}")
            }
            Action::PutOverlay { name, .. } | Action::RemoveOverlay { name } => {
                format!("overlay {name}")
            }
            Action::PutClientClass { class, .. } => format!("client_class {}", class.name),
            Action::RemoveClientClass { name } => format!("client_class {name}"),
            Action::PutRateLimit { name, .. } | Action::RemoveRateLimit { name } => {
                format!("rate_limit {name}")
            }
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
pub fn plan(
    store: &Store,
    document: &Document,
    prune: bool,
    defined: &Defined,
    provisioning: &crate::provision::Provisioning,
) -> Plan {
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

    if document.dashboard.is_some() {
        plan.warnings.push(
            "the [dashboard] table belongs in the daemon configuration file and was not applied"
                .to_owned(),
        );
    }

    if document.overlay_webhook.is_some() {
        plan.warnings.push(
            "the [overlay_webhook] table belongs in the daemon configuration file and was not applied"
                .to_owned(),
        );
    }

    if !document.error_pages.is_empty() || document.error_override.is_some() {
        plan.warnings.push(
            "the node-wide [error_pages] and [error_override] tables belong in the daemon \
             configuration file and were not applied"
                .to_owned(),
        );
    }

    // Sections 4.14 and 4.15: the document's classes and limits come
    // first, so what follows may name them.

    plan_clients(store, document, defined, prune, &mut plan);
    let defined = &defined.with_applied(store, document, prune);

    for (name, app) in &document.app {
        plan_app(store, name, app, &mut plan, provisioning);

        // Section 4.14: a worker group's classes are the daemon's.

        for (id, release) in &app.release {
            for (group, table) in &release.groups {
                let classes = table
                    .matches
                    .as_ref()
                    .map_or(&[][..], |matches| &matches.client_class[..]);

                for class in unknown_among(classes, defined) {
                    plan.errors.push(format!(
                        "{name}@{id}: worker group {group}: no client class named {class}"
                    ));
                }
            }
        }
    }

    plan_hosts(store, document, &defined.rate_limits, &mut plan);
    plan_upstreams(store, document, &mut plan);
    plan_routes(store, document, defined, &mut plan);
    plan_overlays(store, document, defined, prune, &mut plan);
    plan_budget(document, defined, &mut plan);

    if prune {
        plan_removals(store, document, &mut plan);
    }

    plan
}

/// The changes one application's table implies.
fn plan_app(
    store: &Store,
    name: &str,
    app: &AppTable,
    plan: &mut Plan,
    provisioning: &crate::provision::Provisioning,
) {
    if !valid_name(name) {
        plan.errors.push(format!(
            "{name:?} is not a valid application name: lowercase letters, digits and hyphens, up to 63 characters"
        ));
        return;
    }

    let stored = store.app(name).ok();

    if stored.is_none() {
        plan.changes.push(Change {
            action: Action::CreateApp {
                app: name.to_owned(),
            },
            detail: String::new(),
        });
    }

    // Section 10.9: the identity is set whenever the table says
    // something different from the record.

    if let Some(umask) = &app.umask
        && !crate::model::valid_umask(umask)
    {
        plan.errors.push(format!(
            "{name}: umask must be octal digits such as 027, not {umask:?}"
        ));
        return;
    }

    if app.group.is_some() && app.user.is_none() {
        plan.errors
            .push(format!("{name}: a group needs a user as well"));
        return;
    }

    let same = stored.as_ref().is_some_and(|stored| {
        stored.user == app.user && stored.group == app.group && stored.umask == app.umask
    });

    if !same && (stored.is_some() || app.user.is_some() || app.umask.is_some()) {
        plan.changes.push(Change {
            action: Action::SetAppIdentity {
                app: name.to_owned(),
                user: app.user.clone(),
                group: app.group.clone(),
                umask: app.umask.clone(),
            },
            detail: match &app.user {
                Some(user) => format!(
                    "user {user}{}{}",
                    app.group
                        .as_ref()
                        .map(|group| format!(", group {group}"))
                        .unwrap_or_default(),
                    app.umask
                        .as_ref()
                        .map(|umask| format!(", umask {umask}"))
                        .unwrap_or_default()
                ),
                None => "the daemon's own user".to_owned(),
            },
        });
    }

    let priority = app.priority.unwrap_or(0);

    if stored
        .as_ref()
        .map_or(priority != 0, |stored| stored.priority != priority)
    {
        plan.changes.push(Change {
            action: Action::SetAppPriority {
                app: name.to_owned(),
                priority,
            },
            detail: format!("priority {priority}"),
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

        if let Err(error) = muru_config::check_release_kind(table) {
            plan.errors.push(format!("{name}@{id}: {error}"));
            continue;
        }

        // Section 9.9: a managed HTTP release runs its command in cwd,
        // which stands in for the source directory.

        let Some(source) = table.cwd.as_ref().or(table.source.as_ref()) else {
            plan.errors
                .push(format!("{name}@{id} has no source directory"));
            continue;
        };

        let tables = Tables {
            groups: (!table.groups.is_empty()).then_some(&table.groups),
            mounts: (!table.mounts.is_empty()).then_some(&table.mounts),
            static_dirs: (!table.static_dirs.is_empty()).then_some(&table.static_dirs[..]),
            logging: table.logging.as_ref(),
            scaling: table.scaling.as_ref(),
            error_pages: (!table.error_pages.is_empty()).then_some(&table.error_pages),
            error_override: table.error_override.as_ref(),
            maintenance: table.maintenance.as_ref(),
            declared: true,
        };

        let spec = match resolve_spec(
            &request_from(name, table, source),
            None,
            &tables,
            provisioning,
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
                    spec: Box::new(spec),
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

/// What the daemon's file sets that applied files are checked against:
/// the rate limits (section 4.15) and client classes (section 4.14) they
/// may name, the daemon's file's and, with `with_applied`, the applied
/// files' too, and the node's budget with what its releases hold
/// (section 7.4).
#[derive(Debug, Clone, Default)]
pub struct Defined {
    /// The `[rate_limit.NAME]` tables.
    pub rate_limits: Vec<String>,
    /// The classes of the `[[client_class]]` rules.
    pub client_classes: Vec<String>,
    /// The node's budget and what its releases hold.
    pub budget: crate::budget::View,
}

impl Defined {
    /// These names with the classes and limits of applied files: those
    /// `document` declares and, without `prune`, those recorded.
    fn with_applied(&self, store: &Store, document: &Document, prune: bool) -> Defined {
        let mut all = self.clone();
        all.client_classes
            .extend(document.client_class.iter().map(|class| class.name.clone()));
        all.rate_limits.extend(document.rate_limit.keys().cloned());

        if !prune {
            all.client_classes.extend(
                store
                    .client_classes()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(_, class)| class.name),
            );
            all.rate_limits.extend(
                store
                    .rate_limits()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, _)| name),
            );
        }

        all
    }
}

/// Sections 4.14 and 4.15: the changes the document's client classes and
/// rate limits imply. A class's rule keeps its place among the applied
/// ones, a new one going after them, and with `prune` they take the
/// document's order; names the daemon's file defines are refused, as are
/// list files that are not absolute paths.
fn plan_clients(store: &Store, document: &Document, file: &Defined, prune: bool, plan: &mut Plan) {
    let stored = store.client_classes().unwrap_or_default();
    let mut next = stored
        .iter()
        .map(|(position, _)| position + 1)
        .max()
        .unwrap_or(0);
    let mut seen = BTreeSet::new();

    for (index, class) in document.client_class.iter().enumerate() {
        let name = &class.name;

        if !seen.insert(name.clone()) {
            plan.errors
                .push(format!("client_class {name} is declared twice"));
            continue;
        }

        if file.client_classes.contains(name) {
            plan.errors.push(format!(
                "client_class {name}: the daemon configuration file defines it"
            ));
            continue;
        }

        let relative = [&class.user_agent_list, &class.address_list, &class.ja4_list]
            .into_iter()
            .flatten()
            .find(|list| !Path::new(list).is_absolute());

        if let Some(list) = relative {
            plan.errors.push(format!(
                "client_class {name}: the list {list} must be an absolute path"
            ));
            continue;
        }

        // The edge's own parsing, with the lists read, checks the rule.

        let resolved = crate::clients::ClientPolicies::resolve(
            std::slice::from_ref(class),
            &BTreeMap::new(),
            Path::new("/"),
        );

        if let Err(error) = resolved {
            plan.errors.push(error);
            continue;
        }

        let recorded = stored.iter().find(|(_, stored)| &stored.name == name);

        let position = match recorded {
            _ if prune => index as i64,
            Some((position, _)) => *position,
            None => {
                next += 1;
                next - 1
            }
        };

        match recorded {
            Some((at, stored)) if stored == class && *at == position => plan.unchanged += 1,
            recorded => plan.changes.push(Change {
                action: Action::PutClientClass {
                    class: Box::new(class.clone()),
                    position,
                    new: recorded.is_none(),
                },
                detail: describe_class(class),
            }),
        }
    }

    let stored_limits: BTreeMap<String, RateLimit> = store
        .rate_limits()
        .unwrap_or_default()
        .into_iter()
        .collect();

    for (name, limit) in &document.rate_limit {
        if file.rate_limits.contains(name) {
            plan.errors.push(format!(
                "rate_limit {name}: the daemon configuration file defines it"
            ));
            continue;
        }

        if let Err(error) = muru_config::check_rate_limit(name, limit) {
            plan.errors.push(error);
            continue;
        }

        match stored_limits.get(name) {
            Some(recorded) if recorded == limit => plan.unchanged += 1,
            recorded => plan.changes.push(Change {
                action: Action::PutRateLimit {
                    name: name.clone(),
                    limit: Box::new(limit.clone()),
                    new: recorded.is_none(),
                },
                detail: format!(
                    "{}, {}",
                    limit.rate,
                    limit.action.as_deref().unwrap_or("reject")
                ),
            }),
        }
    }
}

/// What a class's rule matches on, as a phrase.
fn describe_class(class: &ClientClass) -> String {
    let inputs = [
        ("user_agent", !class.user_agent.is_empty()),
        ("user_agent_regex", !class.user_agent_regex.is_empty()),
        ("user_agent_list", class.user_agent_list.is_some()),
        ("addresses", !class.addresses.is_empty()),
        ("address_list", class.address_list.is_some()),
        ("ja4", !class.ja4.is_empty()),
        ("ja4_list", class.ja4_list.is_some()),
        ("headers", !class.headers.is_empty()),
        ("cookies", !class.cookies.is_empty()),
    ];
    let named: Vec<&str> = inputs
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(input, _)| input)
        .collect();
    let verified = class.verify.as_ref().map_or("", |_| ", verified");

    format!("matches {}{verified}", named.join(", "))
}

/// Section 4.14: the classes a `match` names that neither the daemon's
/// file nor an applied file defines; `default` is always defined.
fn unknown_classes(matches: Option<&muru_config::ClientMatch>, defined: &Defined) -> Vec<String> {
    unknown_among(
        matches.map_or(&[][..], |matches| &matches.client_class[..]),
        defined,
    )
}

/// The classes among `classes` that nothing defines.
fn unknown_among(classes: &[String], defined: &Defined) -> Vec<String> {
    classes
        .iter()
        .filter(|class| class.as_str() != "default" && !defined.client_classes.contains(class))
        .cloned()
        .collect()
}

/// Section 4.15: the rate limits `names` asks for that the daemon's file
/// does not define.
fn unknown_limits(names: &[String], defined: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|name| !defined.contains(name))
        .cloned()
        .collect()
}

/// Section 7.4: whether the releases the plan deploys, and the floors its
/// schedules and overlays raise, fit the node's budget as it stands;
/// warnings, since the node may hold less or more when they apply.
fn plan_budget(document: &Document, defined: &Defined, plan: &mut Plan) {
    let view = &defined.budget;

    if !view.budget.is_set() {
        return;
    }

    for change in &plan.changes {
        if let Action::Deploy { app, id, spec } = &change.action
            && let Err(why) = view.release_fits(app, spec, view.retained)
        {
            plan.warnings
                .push(format!("{app}@{id}: {why}, so its deploy would be refused"));
        }
    }

    let (schedules, _) = schedule_overlays(document);
    let overlays: Vec<(String, Overlay)> = document
        .overlay
        .clone()
        .into_iter()
        .chain(schedules)
        .collect();

    for entry in &overlays {
        for ((app, group), capacity) in crate::overlays::capacities(std::iter::once(entry)) {
            if capacity.floor == 0 {
                continue;
            }

            if let Err(why) = view.floor_fits(&app, &group, capacity.floor, None) {
                let what = if crate::overlays::is_schedule(&entry.0) {
                    "schedule"
                } else {
                    "overlay"
                };
                plan.warnings.push(format!(
                    "{what} {}: a floor of {} workers for {app} ({group} group) does not fit the node's budget as it stands: {why}",
                    entry.0, capacity.floor
                ));
            }
        }
    }
}

/// The changes the document's hosts imply (section 4.3): names are
/// checked, duplicates refused and certificates read, so a bad host
/// never reaches the edge.
fn plan_hosts(store: &Store, document: &Document, rate_limits: &[String], plan: &mut Plan) {
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

        for missing in unknown_limits(&host.rate_limits, rate_limits) {
            plan.errors
                .push(format!("host {name}: no rate limit named {missing}"));
            valid = false;
        }

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

        // Section 4.12: the host's error pages, with absolute file paths.

        if let Err(error) = check_error_tables(&host.error_pages, host.error_override.as_ref()) {
            plan.errors.push(format!("host {name}: {error}"));
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
fn plan_routes(store: &Store, document: &Document, defined: &Defined, plan: &mut Plan) {
    let stored: BTreeMap<(String, String), RouteTable> = store
        .routes()
        .unwrap_or_default()
        .into_iter()
        .map(|route| ((route.host.clone(), route.key()), route))
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
    let stored_upstreams: Vec<String> = store
        .upstreams()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let known_upstream = |name: &str| {
        document.upstream.contains_key(name) || stored_upstreams.iter().any(|u| u == name)
    };
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut classes_at: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    let mut names: BTreeMap<&str, (&str, String)> = BTreeMap::new();

    // Section 9.10: a route's name is how overlays reach it, so it is
    // unique among the declared routes and those already recorded.

    for route in &stored_routes_named(&stored) {
        names.insert(route.0, (route.1, route.2.clone()));
    }

    for route in &document.route {
        let key = (route.host.clone(), route.key());
        let mut valid = true;

        if let Some(name) = &route.name {
            if !valid_name(name) {
                plan.errors.push(format!(
                    "route {} {}: {name:?} is not a valid name",
                    route.host, route.path
                ));
                valid = false;
            }

            match names.get(name.as_str()) {
                Some((host, path)) if (*host, path) != (route.host.as_str(), &key.1) => {
                    plan.errors.push(format!(
                        "route {} {}: the name {name} is the route {host} {path}'s",
                        route.host, route.path
                    ));
                    valid = false;
                }
                _ => {
                    names.insert(name, (&route.host, key.1.clone()));
                }
            }
        }

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

        // Section 4.9: a route names an application or a directory, and
        // a directory route's settings are checked here, before the edge
        // sees them.

        let fallback_app = route
            .fallback
            .as_ref()
            .and_then(|fallback| fallback.app.as_deref());
        let known_app =
            |app: &str| document.app.contains_key(app) || stored_apps.iter().any(|a| a == app);

        // Section 4.13: an upstream is a target like an application, and
        // a static route may fall back to one.

        let fallback_upstream = route
            .fallback
            .as_ref()
            .and_then(|fallback| fallback.upstream.as_deref());
        let targets = usize::from(!route.app.is_empty())
            + usize::from(!route.upstream.is_empty())
            + usize::from(route.static_dir.is_some());

        if targets > 1 {
            plan.errors.push(format!(
                "route {} {}: names more than one of an application, an upstream and a static directory",
                route.host, route.path
            ));
            valid = false;
        }

        if !route.upstream.is_empty() && !known_upstream(&route.upstream) {
            plan.errors.push(format!(
                "route {} {}: no upstream named {}",
                route.host, route.path, route.upstream
            ));
            valid = false;
        }

        if let Some(upstream) = fallback_upstream
            && !known_upstream(upstream)
        {
            plan.errors.push(format!(
                "route {} {}: no upstream named {upstream} for the fallback",
                route.host, route.path
            ));
            valid = false;
        }

        if fallback_app.is_some() && fallback_upstream.is_some() {
            plan.errors.push(format!(
                "route {} {}: the fallback names both an application and an upstream",
                route.host, route.path
            ));
            valid = false;
        }

        match (&route.static_dir, route.app.is_empty()) {
            (Some(_), false) => {}
            (None, true) if route.upstream.is_empty() => {
                plan.errors.push(format!(
                    "route {} {}: names neither an application, an upstream nor a static directory",
                    route.host, route.path
                ));
                valid = false;
            }
            (None, true) => {
                if route.fallback.is_some() || route.cache.is_some() || !route.index.is_empty() {
                    plan.warnings.push(format!(
                        "route {} {}: index, fallback and cache apply to static routes only",
                        route.host, route.path
                    ));
                }

                if route.traffic.is_some() {
                    plan.errors.push(format!(
                        "route {} {}: traffic needs an application; an upstream route has no releases to share",
                        route.host, route.path
                    ));
                    valid = false;
                }
            }
            (Some(directory), true) => {
                if !Path::new(directory).is_dir() {
                    plan.errors.push(format!(
                        "route {} {}: {directory} is not a directory",
                        route.host, route.path
                    ));
                    valid = false;
                }

                if let Some(app) = fallback_app
                    && !known_app(app)
                {
                    plan.errors.push(format!(
                        "route {} {}: no application named {app} for the fallback",
                        route.host, route.path
                    ));
                    valid = false;
                }

                if let Some(text) = route
                    .cache
                    .as_ref()
                    .and_then(|cache| cache.max_age.as_deref())
                    && let Err(error) = muru_config::parse_duration(text)
                {
                    plan.errors.push(format!(
                        "route {} {}: cache.max_age: {error}",
                        route.host, route.path
                    ));
                    valid = false;
                }

                if let Some(case) = route.case.as_deref()
                    && !["strict", "insensitive"].contains(&case)
                {
                    plan.errors.push(format!(
                        "route {} {}: case must be strict or insensitive, not {case:?}",
                        route.host, route.path
                    ));
                    valid = false;
                }
            }
            (None, false) => {
                if !known_app(&route.app) {
                    plan.errors.push(format!(
                        "route {} {}: no application named {}",
                        route.host, route.path, route.app
                    ));
                    valid = false;
                }

                if route.fallback.is_some() || route.cache.is_some() || !route.index.is_empty() {
                    plan.warnings.push(format!(
                        "route {} {}: index, fallback and cache apply to static routes only",
                        route.host, route.path
                    ));
                }
            }
        }

        // Section 4.15: the route's rate limits, method restrictions and
        // mode.

        if let Err(error) = muru_config::check_route_policies(route) {
            plan.errors
                .push(format!("route {} {}: {error}", route.host, route.path));
            valid = false;
        }

        for missing in unknown_limits(&route.rate_limits, &defined.rate_limits) {
            plan.errors.push(format!(
                "route {} {}: no rate limit named {missing}",
                route.host, route.path
            ));
            valid = false;
        }

        // Section 4.14: the classes a route and its policy are for are
        // the daemon's, and two routes with one path are for different
        // classes.

        let pinned = route
            .traffic
            .iter()
            .flat_map(|traffic| traffic.classes.keys().cloned())
            .collect::<Vec<_>>();
        let named = muru_config::ClientMatch {
            client_class: pinned,
        };

        for class in unknown_classes(route.matches.as_ref(), defined)
            .into_iter()
            .chain(unknown_classes(Some(&named), defined))
        {
            plan.errors.push(format!(
                "route {} {}: no client class named {class}",
                route.host, route.path
            ));
            valid = false;
        }

        let at = classes_at
            .entry((route.host.as_str(), route.path.as_str()))
            .or_default();

        if let Some(class) = route
            .classes()
            .iter()
            .find(|class| at.contains(&class.as_str()))
        {
            plan.errors.push(format!(
                "route {} {}: another route with the path is for client class {class}",
                route.host, route.path
            ));
            valid = false;
        }

        at.extend(route.classes().iter().map(String::as_str));

        // Sections 4.6 and 4.11: the body limit, and the upload keys, which
        // need an application to hand the files to.

        match muru_config::check_upload(route) {
            Ok(Some(_)) if route.app.is_empty() => {
                plan.errors.push(format!(
                    "route {} {}: upload needs an application; static and upstream routes take none",
                    route.host, route.path
                ));
                valid = false;
            }
            Ok(_) => {}
            Err(error) => {
                plan.errors
                    .push(format!("route {} {}: {error}", route.host, route.path));
                valid = false;
            }
        }

        // Section 9.4: a traffic policy names releases of the route's
        // application that exist or are declared, with shares that fit.

        if let Some(traffic) = &route.traffic {
            let app = if route.app.is_empty() {
                fallback_app
            } else {
                Some(route.app.as_str())
            };

            match app {
                None => {
                    plan.errors.push(format!(
                        "route {} {}: traffic needs an application; a static route takes it through its fallback",
                        route.host, route.path
                    ));
                    valid = false;
                }
                Some(app) => {
                    for id in traffic.split.keys().chain(traffic.classes.values()) {
                        // Section 4.13: a share may go to an upstream.

                        if let Some(upstream) = id.strip_prefix(muru_edge::routing::UPSTREAM_PREFIX)
                        {
                            if !known_upstream(upstream) {
                                plan.errors.push(format!(
                                    "route {} {}: traffic names upstream {upstream}, which does not exist",
                                    route.host, route.path
                                ));
                                valid = false;
                            }

                            continue;
                        }

                        let declared = document
                            .app
                            .get(app)
                            .is_some_and(|table| table.release.contains_key(id));

                        if !declared && store.release(app, id).is_err() {
                            plan.errors.push(format!(
                                "route {} {}: traffic names {app}@{id}, which does not exist",
                                route.host, route.path
                            ));
                            valid = false;
                        }
                    }
                }
            }

            if traffic.split.values().sum::<u32>() > 100 {
                plan.errors.push(format!(
                    "route {} {}: traffic.split adds up to more than 100 percent",
                    route.host, route.path
                ));
                valid = false;
            }

            if let Some(key) = traffic.hash_key.as_deref()
                && !matches!(key, "cookie" | "client")
                && !matches!(key.split_once(':'), Some(("header" | "cookie", name)) if !name.is_empty())
            {
                plan.errors.push(format!(
                    "route {} {}: traffic.hash_key must be cookie, header:NAME, cookie:NAME or client, not {key:?}",
                    route.host, route.path
                ));
                valid = false;
            }

            if let Some(text) = traffic.sticky_grace.as_deref()
                && let Err(error) = muru_config::parse_duration(text)
            {
                plan.errors.push(format!(
                    "route {} {}: traffic.sticky_grace: {error}",
                    route.host, route.path
                ));
                valid = false;
            }
        }

        // Section 4.12: the route's error pages.

        if let Err(error) = check_error_tables(&route.error_pages, route.error_override.as_ref()) {
            plan.errors
                .push(format!("route {} {}: {error}", route.host, route.path));
            valid = false;
        }

        // Section 4.16: the check's handler must be an application, and
        // the table well formed.

        if let Some(auth) = &route.auth {
            if let Err(error) = muru_config::check_auth(auth) {
                plan.errors
                    .push(format!("route {} {}: {error}", route.host, route.path));
                valid = false;
            } else if !auth.app.is_empty() && !known_app(&auth.app) {
                plan.errors.push(format!(
                    "route {} {}: auth.app names {}, which is not an application",
                    route.host, route.path, auth.app
                ));
                valid = false;
            } else if !auth.upstream.is_empty() && !known_upstream(&auth.upstream) {
                plan.errors.push(format!(
                    "route {} {}: auth.upstream names {}, which is not an upstream",
                    route.host, route.path, auth.upstream
                ));
                valid = false;
            }
        }

        // Section 4.10: the applications, or trusted upstreams (section
        // 4.13), a route accepts redirects from must exist.

        for app in &route.accept_redirects_from {
            if !known_app(app) && !known_upstream(app) {
                plan.errors.push(format!(
                    "route {} {}: accept_redirects_from names {app}, which is neither an application nor an upstream",
                    route.host, route.path
                ));
                valid = false;
            }
        }

        if seen.contains(&key) {
            plan.errors
                .push(format!("route {} {} is declared twice", route.host, key.1));
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
                    route: Box::new(route.clone()),
                    new: recorded.is_none(),
                },
                detail: {
                    let target = if route.upstream.is_empty() {
                        route.app.clone()
                    } else {
                        format!("upstream {}", route.upstream)
                    };

                    let mut detail = match &route.traffic {
                        Some(traffic) => {
                            format!("to {target}, {}", describe_traffic(Some(traffic)))
                        }
                        None => format!("to {target}"),
                    };

                    if route.internal == Some(true) {
                        detail.push_str(", internal");
                    }

                    if !route.accept_redirects_from.is_empty() {
                        detail.push_str(&format!(
                            ", redirects from {}",
                            route.accept_redirects_from.join(", ")
                        ));
                    }

                    if let Some(auth) = &route.auth {
                        detail.push_str(&format!(", auth by {} {}", auth_handler(auth), auth.path));
                    }

                    detail
                },
            }),
        }
    }
}

/// The handler of a check as plans and listings name it: the
/// application, or `upstream NAME`.
pub fn auth_handler(auth: &muru_config::Auth) -> String {
    if auth.upstream.is_empty() {
        auth.app.clone()
    } else {
        format!("upstream {}", auth.upstream)
    }
}

/// The changes the document's HTTP upstreams imply (section 4.13).
/// The recorded routes with names, as (name, host, key).
fn stored_routes_named(
    stored: &BTreeMap<(String, String), RouteTable>,
) -> Vec<(&str, &str, String)> {
    stored
        .values()
        .filter_map(|route| {
            route
                .name
                .as_deref()
                .map(|name| (name, route.host.as_str(), route.key()))
        })
        .collect()
}

/// Section 9.10: the overlays a document declares, checked against the
/// routes, applications, upstreams and rate limits they name, and
/// against each other.
fn plan_overlays(
    store: &Store,
    document: &Document,
    defined: &Defined,
    prune: bool,
    plan: &mut Plan,
) {
    let stored: BTreeMap<String, Overlay> =
        store.overlays().unwrap_or_default().into_iter().collect();
    let stored_routes = store.routes().unwrap_or_default();
    let mut route_names: BTreeMap<String, String> = document
        .route
        .iter()
        .filter_map(|route| route.name.clone().map(|name| (name, route.app.clone())))
        .collect();

    if !prune {
        for route in &stored_routes {
            if let Some(name) = &route.name {
                route_names
                    .entry(name.clone())
                    .or_insert_with(|| route.app.clone());
            }
        }
    }

    let stored_apps: Vec<String> = store
        .apps()
        .unwrap_or_default()
        .into_iter()
        .map(|app| app.name)
        .collect();
    let known_app =
        |app: &str| document.app.contains_key(app) || stored_apps.iter().any(|name| name == app);
    let stored_upstreams: Vec<String> = store
        .upstreams()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let known_upstream = |name: &str| {
        document.upstream.contains_key(name) || stored_upstreams.iter().any(|u| u == name)
    };

    // Section 7.5: each application's schedules are overlays named
    // APP.NAME, checked and recorded as the declared ones are.

    let (schedules, schedule_errors) = schedule_overlays(document);
    plan.errors.extend(schedule_errors);

    for (name, overlay) in document.overlay.iter().chain(schedules.iter()) {
        if !valid_name(name) && !schedules.contains_key(name) {
            plan.errors
                .push(format!("overlay {name:?} is not a valid name"));
            continue;
        }

        let mut errors = Vec::new();

        if let Err(error) = muru_config::check_overlay(name, overlay) {
            errors.push(error);
        }

        for (route, change) in &overlay.routes {
            if !route_names.contains_key(route) {
                errors.push(format!("overlay {name}: no route is named {route}"));
            }

            // The route's own changes, then each route for a class.

            let changes = std::iter::once((format!("routes.{route}"), change)).chain(
                change
                    .client_routes
                    .iter()
                    .flatten()
                    .map(|(class, class_change)| {
                        (
                            format!("routes.{route}.client_routes.{class}"),
                            class_change,
                        )
                    }),
            );

            for (field, change) in changes {
                for limit in change.rate_limits.iter().flatten() {
                    if !defined.rate_limits.contains(limit) {
                        errors.push(format!(
                            "overlay {name}: {field}: no rate limit named {limit}"
                        ));
                    }
                }

                let Some(target) = &change.target else {
                    continue;
                };

                if let Some(app) = &target.app
                    && !known_app(app)
                {
                    errors.push(format!(
                        "overlay {name}: {field}: no application named {app}"
                    ));
                }

                if let Some(upstream) = &target.upstream
                    && !known_upstream(upstream)
                {
                    errors.push(format!(
                        "overlay {name}: {field}: no upstream named {upstream}"
                    ));
                }

                if let Some(directory) = &target.static_dir
                    && !Path::new(directory).is_dir()
                {
                    errors.push(format!(
                        "overlay {name}: {field}: {directory} is not a directory"
                    ));
                }
            }

            for class in change.client_routes.iter().flat_map(BTreeMap::keys) {
                if class != "default" && !defined.client_classes.contains(class) {
                    errors.push(format!(
                        "overlay {name}: routes.{route}.client_routes: no client class named {class}"
                    ));
                }
            }
        }

        for app in overlay.apps.keys() {
            if !known_app(app) {
                errors.push(format!("overlay {name}: no application named {app}"));
            }
        }

        if let Some(ready) = overlay
            .preconditions
            .as_ref()
            .and_then(|preconditions| preconditions.release_ready.as_deref())
            && precondition_release(ready, overlay, &route_names).is_none()
        {
            errors.push(format!(
                "overlay {name}: release_ready {ready:?} must be app@release, or a release when the overlay changes routes of one application"
            ));
        }

        if let Some(release) = overlay
            .warm_up
            .as_ref()
            .and_then(|warm_up| warm_up.start_release.as_deref())
            && precondition_release(release, overlay, &route_names).is_none()
        {
            errors.push(format!(
                "overlay {name}: warm_up.start_release {release:?} must be app@release, or a release when the overlay changes routes of one application"
            ));
        }

        if !errors.is_empty() {
            plan.errors.extend(errors);
            continue;
        }

        match stored.get(name) {
            Some(recorded) if recorded == overlay => plan.unchanged += 1,
            recorded => plan.changes.push(Change {
                action: Action::PutOverlay {
                    name: name.clone(),
                    overlay: Box::new(overlay.clone()),
                    new: recorded.is_none(),
                },
                detail: describe_overlay(overlay),
            }),
        }
    }

    // Overlays that would set the same field need priorities, among the
    // declared ones and those already recorded that stay.

    let mut all = document.overlay.clone();
    all.extend(schedules);

    if !prune {
        for (name, overlay) in stored {
            all.entry(name).or_insert(overlay);
        }
    }

    plan.errors.extend(crate::overlays::conflicts(&all));
}

/// Section 7.5: the overlays the applications' `[[app.NAME.schedule]]`
/// entries stand for, by name, and what is wrong with the entries.
fn schedule_overlays(document: &Document) -> (BTreeMap<String, Overlay>, Vec<String>) {
    let mut overlays = BTreeMap::new();
    let mut errors = Vec::new();

    for (app, table) in &document.app {
        for schedule in &table.schedule {
            if !valid_name(&schedule.name) {
                errors.push(format!(
                    "app {app}: schedule {:?} is not a valid name",
                    schedule.name
                ));
                continue;
            }

            let name = format!("{app}.{}", schedule.name);

            if overlays.insert(name, schedule.overlay(app)).is_some() {
                errors.push(format!(
                    "app {app}: two schedules are named {}",
                    schedule.name
                ));
            }
        }
    }

    (overlays, errors)
}

/// The release an overlay's `release_ready` names, as (app, release):
/// app@release, or a release of the one application its routes belong to.
pub fn precondition_release(
    ready: &str,
    overlay: &Overlay,
    route_apps: &BTreeMap<String, String>,
) -> Option<(String, String)> {
    if let Some((app, release)) = ready.split_once('@') {
        return (!app.is_empty() && !release.is_empty())
            .then(|| (app.to_owned(), release.to_owned()));
    }

    let apps: BTreeSet<&String> = overlay
        .routes
        .keys()
        .filter_map(|route| route_apps.get(route))
        .filter(|app| !app.is_empty())
        .collect();

    match apps.into_iter().collect::<Vec<_>>().as_slice() {
        [app] => Some(((*app).clone(), ready.to_owned())),
        _ => None,
    }
}

/// An overlay for the report: what triggers it and what it changes.
fn describe_overlay(overlay: &Overlay) -> String {
    let mut parts = Vec::new();

    parts.push(crate::overlays::describe_trigger(overlay));

    if !overlay.routes.is_empty() {
        parts.push(format!(
            "routes {}",
            overlay
                .routes
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if !overlay.apps.is_empty() {
        parts.push(format!(
            "apps {}",
            overlay.apps.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    parts.join("; ")
}

fn plan_upstreams(store: &Store, document: &Document, plan: &mut Plan) {
    let stored: BTreeMap<String, UpstreamTable> =
        store.upstreams().unwrap_or_default().into_iter().collect();

    for (name, upstream) in &document.upstream {
        if !valid_name(name) {
            plan.errors
                .push(format!("upstream {name:?} is not a valid name"));
            continue;
        }

        let mut valid = true;

        if let Err(error) = muru_config::check_upstream(upstream) {
            plan.errors.push(format!("upstream {name}: {error}"));
            valid = false;
        }

        if let Some(ca) = upstream.tls.as_ref().and_then(|tls| tls.ca.as_deref())
            && let Err(error) = std::fs::read_to_string(ca)
        {
            plan.errors
                .push(format!("upstream {name}: tls.ca: reading {ca}: {error}"));
            valid = false;
        }

        if !valid {
            continue;
        }

        match stored.get(name) {
            Some(recorded) if recorded == upstream => plan.unchanged += 1,
            recorded => plan.changes.push(Change {
                action: Action::PutUpstream {
                    name: name.clone(),
                    upstream: Box::new(upstream.clone()),
                    new: recorded.is_none(),
                },
                detail: describe_upstream(upstream),
            }),
        }
    }
}

/// An upstream for the report: its addresses, protocol and checks.
fn describe_upstream(upstream: &UpstreamTable) -> String {
    let mut parts = vec![
        upstream.addresses.join(", "),
        upstream
            .protocol
            .clone()
            .unwrap_or_else(|| "http1".to_owned()),
    ];

    if upstream.tls.is_some() {
        parts.push("with TLS".to_owned());
    }

    if let Some(path) = upstream
        .health
        .as_ref()
        .and_then(|health| health.path.as_deref())
    {
        parts.push(format!("probing {path}"));
    }

    if upstream.trust_muru_headers {
        parts.push("trusting X-Muru headers".to_owned());
    }

    parts.join(", ")
}

/// Section 4.12: a host's or route's error pages and override, whose
/// files must be absolute paths to files that exist, so a missing page is
/// refused here rather than by the edge when the snapshot reaches it.
fn check_error_tables(
    pages: &BTreeMap<String, muru_config::ErrorPage>,
    override_: Option<&muru_config::ErrorOverride>,
) -> Result<(), String> {
    muru_config::check_error_pages(pages, false)?;

    for (key, page) in pages {
        if let Some(file) = &page.file
            && !Path::new(file).is_file()
        {
            return Err(format!("error_pages: {key}: {file} is not a file"));
        }
    }

    if let Some(table) = override_ {
        muru_config::check_error_override(table)?;
    }

    Ok(())
}

/// A traffic policy in words, for plans and events: the shares, the
/// stickiness and the key.
pub fn describe_traffic(traffic: Option<&muru_config::Traffic>) -> String {
    let Some(traffic) = traffic else {
        return "every request to the serving release".to_owned();
    };
    let mut parts: Vec<String> = traffic
        .split
        .iter()
        .map(|(release, percent)| format!("{release} {percent}%"))
        .collect();

    if parts.is_empty() && traffic.classes.is_empty() {
        parts.push("no shares".to_owned());
    }

    parts.extend(
        traffic
            .classes
            .iter()
            .map(|(class, release)| format!("{class} to {release}")),
    );

    if traffic.sticky == Some(true) {
        parts.push("sticky".to_owned());
    }

    if let Some(key) = &traffic.hash_key {
        parts.push(format!("by {key}"));
    }

    parts.join(", ")
}

/// With prune, routes, hosts, releases and applications in desired state
/// that the document does not declare go, in that order.
fn plan_removals(store: &Store, document: &Document, plan: &mut Plan) {
    for route in store.routes().unwrap_or_default() {
        let declared = document
            .route
            .iter()
            .any(|candidate| candidate.host == route.host && candidate.key() == route.key());

        if !declared {
            plan.changes.push(Change {
                action: Action::RemoveRoute {
                    path: route.key(),
                    host: route.host,
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

    for (_, class) in store.client_classes().unwrap_or_default() {
        if !document
            .client_class
            .iter()
            .any(|declared| declared.name == class.name)
        {
            plan.changes.push(Change {
                action: Action::RemoveClientClass { name: class.name },
                detail: "not declared".to_owned(),
            });
        }
    }

    for (name, _) in store.rate_limits().unwrap_or_default() {
        if !document.rate_limit.contains_key(&name) {
            plan.changes.push(Change {
                action: Action::RemoveRateLimit { name },
                detail: "not declared".to_owned(),
            });
        }
    }

    for (name, _) in store.upstreams().unwrap_or_default() {
        if !document.upstream.contains_key(&name) {
            plan.changes.push(Change {
                action: Action::RemoveUpstream { name },
                detail: "not declared".to_owned(),
            });
        }
    }

    let (schedules, _) = schedule_overlays(document);

    for (name, _) in store.overlays().unwrap_or_default() {
        if !document.overlay.contains_key(&name)
            && !schedules.contains_key(&name)
            && !crate::overlays::is_temporary(&name)
        {
            plan.changes.push(Change {
                action: Action::RemoveOverlay { name },
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
        let mut table = AppTable {
            user: app.user.clone(),
            group: app.group.clone(),
            umask: app.umask.clone(),
            priority: (app.priority != 0).then_some(app.priority),
            ..AppTable::default()
        };

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

    // Upstreams are node-wide too; with one application asked for, those
    // its routes name come along.

    for (name, upstream) in store.upstreams()? {
        let share = format!("{}{name}", muru_edge::routing::UPSTREAM_PREFIX);
        let named = document.route.iter().any(|route| {
            route.upstream == name
                || route
                    .fallback
                    .as_ref()
                    .is_some_and(|fallback| fallback.upstream.as_deref() == Some(&name))
                || route
                    .traffic
                    .as_ref()
                    .is_some_and(|traffic| traffic.split.contains_key(&share))
                || route
                    .auth
                    .as_ref()
                    .is_some_and(|auth| auth.upstream == name)
        });

        if app.is_none() || named {
            document.upstream.insert(name, upstream);
        }
    }

    // Applied client classes and rate limits (sections 4.14 and 4.15),
    // in their order; with one application asked for, the limits its
    // hosts and routes draw from and the classes they, its worker groups
    // and those limits name.

    let limits: Vec<(String, muru_config::RateLimit)> = store
        .rate_limits()?
        .into_iter()
        .filter(|(name, _)| {
            app.is_none()
                || document
                    .route
                    .iter()
                    .any(|route| route.rate_limits.contains(name))
                || document
                    .host
                    .values()
                    .any(|host| host.rate_limits.contains(name))
        })
        .collect();
    let mut named: BTreeSet<&str> = BTreeSet::new();

    for route in &document.route {
        named.extend(route.classes().iter().map(String::as_str));
        named.extend(
            route
                .traffic
                .iter()
                .flat_map(|traffic| traffic.classes.keys().map(String::as_str)),
        );
    }

    for table in document.app.values() {
        for release in table.release.values() {
            for group in release.groups.values() {
                named.extend(
                    group
                        .matches
                        .iter()
                        .flat_map(|matches| matches.client_class.iter().map(String::as_str)),
                );
            }
        }
    }

    for (_, limit) in &limits {
        named.extend(
            limit
                .matches
                .iter()
                .flat_map(|matches| matches.client_class.iter().map(String::as_str)),
        );
    }

    let classes: Vec<muru_config::ClientClass> = store
        .client_classes()?
        .into_iter()
        .map(|(_, class)| class)
        .filter(|class| app.is_none() || named.contains(class.name.as_str()))
        .collect();
    document.client_class = classes;
    document.rate_limit = limits.into_iter().collect();

    // Overlays (section 9.10): schedules go back to their applications'
    // tables, and with one application asked for, only the overlays that
    // change it or its routes come along. Those muructl scale made are
    // not configuration.

    for (name, overlay) in store.overlays()? {
        if crate::overlays::is_temporary(&name) {
            continue;
        }

        if let Some((owner, schedule)) =
            muru_config::CapacitySchedule::from_overlay(&name, &overlay)
        {
            if let Some(table) = document.app.get_mut(&owner) {
                table.schedule.push(schedule);
            }

            continue;
        }

        let relevant = app.is_none_or(|app| {
            overlay.apps.contains_key(app)
                || overlay.routes.keys().any(|route| {
                    document
                        .route
                        .iter()
                        .any(|declared| declared.name.as_deref() == Some(route))
                })
        });

        if relevant {
            document.overlay.insert(name, overlay);
        }
    }

    Ok(document)
}

/// Section 10.4: write each rollout under way in an exported document as
/// where it stands: its candidate declared beside the baseline, and each
/// of the application's routes splitting traffic between them at the
/// candidate's share in force, the baseline taking what the other shares
/// leave, so the file gives the same split whichever release serves.
pub fn resolve_rollouts(store: &Store, document: &mut Document) -> Result<(), StoreError> {
    for rollout in store.rollouts(None)? {
        let Some(table) = document.app.get_mut(&rollout.app) else {
            continue;
        };

        if !rollout.state.is_active() {
            continue;
        }

        for id in [&rollout.baseline, &rollout.candidate] {
            if !table.release.contains_key(id) {
                let release = store.release(&rollout.app, id)?;
                table.release.insert(id.clone(), table_from(&release.spec));
            }
        }

        for route in document
            .route
            .iter_mut()
            .filter(|route| route.app == rollout.app)
        {
            let traffic = route.traffic.get_or_insert_with(Default::default);
            traffic
                .split
                .insert(rollout.candidate.clone(), rollout.percent().min(100));

            if !traffic.split.contains_key(&rollout.baseline) {
                let named: u32 = traffic.split.values().sum();
                traffic
                    .split
                    .insert(rollout.baseline.clone(), 100u32.saturating_sub(named));
            }
        }
    }

    Ok(())
}

/// Section 9.10: turn an exported document into the effective one, its
/// routes as the active overlays leave them and without the overlays,
/// and say what each sets that is in force, for `app` alone when given.
pub fn make_effective(
    store: &Store,
    document: &mut Document,
    app: Option<&str>,
) -> Result<Vec<String>, StoreError> {
    let overlays = store.overlays()?;
    let states = store.overlay_states()?;
    let active = crate::overlays::ordered(&overlays, &states);
    let maintenance: BTreeSet<String> = store
        .maintenance()?
        .into_iter()
        .map(|(app, _)| app)
        .collect();
    let routes = store.routes()?;
    let attributions = crate::overlays::attribute(&routes, &maintenance, &active);
    crate::overlays::apply_routes(&mut document.route, &active);
    document.overlay.clear();

    // With one application asked for, only what is about it or its
    // routes.

    let Some(app) = app else {
        return Ok(attributions);
    };

    let subjects: Vec<String> = routes
        .iter()
        .filter(|route| route.app == app)
        .filter_map(|route| route.name.as_ref().map(|name| format!("route {name}: ")))
        .chain([format!("app {app}: ")])
        .collect();
    Ok(attributions
        .into_iter()
        .filter(|line| subjects.iter().any(|subject| line.starts_with(subject)))
        .collect())
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

/// The sub-tables of a declared release (`muructl apply`), which take
/// precedence over the source's own [tool.muru] table; a deploy gives
/// none.
#[derive(Default, Clone, Copy)]
pub struct Tables<'a> {
    /// Worker groups (section 7.1).
    pub groups: Option<&'a BTreeMap<String, GroupTable>>,
    /// Mounts (section 6.7).
    pub mounts: Option<&'a BTreeMap<String, MountTable>>,
    /// The release's own directories of files (section 4.9).
    pub static_dirs: Option<&'a [muru_config::StaticDir]>,
    /// Logging (section 11.4).
    pub logging: Option<&'a muru_config::Logging>,
    /// Scaling (section 7.2).
    pub scaling: Option<&'a muru_config::Scaling>,
    /// Error pages (section 4.12).
    pub error_pages: Option<&'a BTreeMap<String, muru_config::ErrorPage>>,
    /// The error override (section 4.12).
    pub error_override: Option<&'a muru_config::ErrorOverride>,
    /// Maintenance exemptions (section 4.12).
    pub maintenance: Option<&'a muru_config::Maintenance>,
    /// Whether the request is a declared release's table rather than a
    /// deploy's flags, which names its layer (section 3.3).
    pub declared: bool,
}

/// A group's paths as the edge matches them: prefixes without their
/// `*`, and `/` alone for a group that claims every path.
fn group_prefixes(paths: &[String]) -> Vec<String> {
    if paths.is_empty() {
        return vec!["/".to_owned()];
    }

    paths
        .iter()
        .map(|path| muru_edge::routing::group_prefix(path))
        .collect()
}

/// Section 3.3: the layer a release setting came from, the first of
/// the request, the source's [tool.muru] table and the previous release
/// that gives it, else the default.
fn layer(given: bool, table: bool, previous: bool, declared: bool) -> &'static str {
    match (given, table, previous) {
        (true, _, _) if declared => "the release table",
        (true, _, _) => "deploy flags",
        (_, true, _) => "[tool.muru]",
        (_, _, true) => "the previous release",
        _ => "default",
    }
}

/// Resolve a deploy request into a release spec: the request's settings,
/// then the source's [tool.muru] table, then the previous release when
/// one is given (a deploy that is not fresh) and it ran native workers,
/// then the defaults of section 13.1. Every setting a deploy flag can
/// set layers that way; the tables (groups, mounts, logging, scaling and
/// error pages) come from the source alone, and the environment layers
/// by name (section 3.3).
pub fn resolve_spec(
    request: &pb::DeployRequest,
    previous: Option<&ReleaseSpec>,
    tables: &Tables<'_>,
    provisioning: &crate::provision::Provisioning,
) -> Result<ReleaseSpec, String> {
    // Section 9.9: a managed server's release has no workers, entrypoint
    // or interpreter of its own, so the native release after it, the
    // cutover, starts from the defaults.

    let previous = previous.filter(|previous| previous.http.is_none());
    let Tables {
        groups,
        mounts,
        static_dirs,
        logging,
        scaling,
        ..
    } = *tables;
    let source = Path::new(&request.source);

    if !source.is_dir() {
        return Err(format!("{} is not a directory", source.display()));
    }

    let source = source
        .canonicalize()
        .map_err(|error| format!("{}: {error}", request.source))?;

    // Section 9.9: the request's keys checked against its kind; a
    // managed HTTP release has a command instead of an entrypoint and
    // takes nothing from a [tool.muru] table.

    let kind_table = kind_table_from(request)?;
    muru_config::check_release_kind(&kind_table)?;

    if kind_table.kind == Some(muru_config::ReleaseKind::Http) {
        return resolve_http_spec(request, &kind_table, source, tables);
    }

    let table = muru_config::load_project(&source)
        .map_err(|error| error.to_string())?
        .map(|(_, table)| table)
        .unwrap_or_default();

    // The entrypoint from the request replaces the table's with its
    // factory settings, as muructl run does (section 13.1).

    let (entrypoint, from_table, from_previous) = if !request.entrypoint.is_empty() {
        (request.entrypoint.clone(), false, false)
    } else if let Some(entrypoint) = table.entrypoint.clone() {
        (entrypoint, true, false)
    } else if let Some(previous) = previous {
        (previous.entrypoint.clone(), false, true)
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
        (None, _) if from_previous => previous.and_then(|previous| previous.factory),
        (None, _) => None,
    };

    // Section 8.2: the build table from the flags over the declaration
    // over the source's own, and whether the daemon builds for this
    // release: the mode says so and the source has a lock.

    let build = resolve_build(request, &table, &source, provisioning)?;

    // Section 8.1: the interpreter as given, else the source's own
    // virtualenv, else a request resolved through uv, else what the
    // previous release ran, else the python3 on the path; probed, so an
    // interpreter that cannot run Muru fails the deploy here. A release
    // that builds takes a request over the source's virtualenv, since the
    // request says what to build with.

    let (python, interpreter) = resolve_interpreter(
        request,
        &table,
        previous,
        &source,
        provisioning,
        build.builds,
    )?;

    // Section 8.2: where bytecode goes, and whether the source already
    // holds bytecode for the interpreter, which a redirected release then
    // runs on instead.

    let bytecode_mode = if request.bytecode.is_empty() {
        table.bytecode.unwrap_or_default()
    } else {
        request
            .bytecode
            .parse()
            .map_err(|error: String| format!("--bytecode {error}"))?
    };
    let bytecode = crate::model::BytecodeSpec {
        mode: bytecode_mode,
        precompiled: interpreter
            .as_ref()
            .filter(|interpreter| !interpreter.cache_tag.is_empty())
            .is_some_and(|interpreter| {
                muru_provision::build::precompiled(&source, &interpreter.cache_tag)
            }),
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
        .or(previous.map(|previous| previous.sync_threads))
        .unwrap_or(8);
    let concurrency = request
        .concurrency
        .or(table.concurrency.map(|value| value as u32))
        .or(previous.map(|previous| previous.concurrency))
        .unwrap_or(1000);
    let mut resolved_groups = Vec::new();

    for (name, group) in groups.unwrap_or(&table.groups) {
        if !crate::model::valid_name(name) || name == "default" {
            return Err(format!("{name:?} is not a valid worker group name"));
        }

        muru_config::check_group_match(group.matches.as_ref())
            .map_err(|error| format!("worker group {name}: {error}"))?;
        let matches = group.matches.clone().unwrap_or_default();
        let client_class = matches.client_class;

        // ADR 0033: hosts are compared as the host tables hold them, with
        // no wildcard, and methods in upper case.

        let mut host = Vec::new();

        for given in &matches.host {
            let normalised = muru_edge::routing::normalise(given);

            if normalised.is_empty() || normalised.contains('*') || normalised.contains('/') {
                return Err(format!(
                    "worker group {name}: match.host {given:?} is not a host name"
                ));
            }

            host.push(normalised);
        }

        let mut method = Vec::new();

        for given in &matches.method {
            let token = given.trim().to_ascii_uppercase();

            if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_alphabetic()) {
                return Err(format!(
                    "worker group {name}: match.method {given:?} is not a method"
                ));
            }

            method.push(token);
        }

        if group.paths.is_empty() && group.matches.is_none() {
            return Err(format!("worker group {name} has no paths and no match"));
        }

        if let Some(path) = group.paths.iter().find(|path| !path.starts_with('/')) {
            return Err(format!(
                "worker group {name}: {path:?} does not start with /"
            ));
        }

        if group.processes == Some(0) {
            return Err(format!("worker group {name} has no processes"));
        }

        let (group_scaling, group_processes) = resolve_scaling(
            group.scaling.as_ref(),
            group.processes.map_or(processes, |value| value as u32),
            &format!("worker group {name}: scaling"),
        )?;

        if group_scaling.host_idle_timeout_ms.is_some() {
            return Err(format!(
                "worker group {name}: host_idle_timeout belongs in the release's own scaling table"
            ));
        }

        // Section 7.1: the group's own timeouts, checked against the
        // release's once those are resolved below, and its memory policy,
        // resolved over the release's there too.

        muru_config::check_group_timeouts(group.timeouts.as_ref())
            .map_err(|error| format!("worker group {name}: {error}"))?;
        let own = group.timeouts.clone().unwrap_or_default();
        let parse = |text: Option<&String>, key: &str| {
            parse_ms(text, key).map_err(|error| format!("worker group {name}: timeouts.{error}"))
        };
        let group_timeouts = crate::model::GroupTimeouts {
            soft_timeout_ms: parse(own.soft_timeout.as_ref(), "soft_timeout")?,
            hard_timeout_ms: parse(own.hard_timeout.as_ref(), "hard_timeout")?,
            hard_drain_grace_ms: parse(own.hard_drain_grace.as_ref(), "hard_drain_grace")?,
            loop_stall_timeout_ms: parse(own.loop_stall_timeout.as_ref(), "loop_stall_timeout")?,
            gil_stall_timeout_ms: parse(own.gil_stall_timeout.as_ref(), "gil_stall_timeout")?,
        };

        resolved_groups.push(GroupSpec {
            name: name.clone(),
            paths: group.paths.clone(),
            mounts: group.mounts.clone(),
            client_class,
            host,
            method,
            optional: group.optional.unwrap_or(false),
            processes: group_processes,
            threads: group.threads.map_or(threads, |value| value as u32),
            sync_threads: group
                .sync_threads
                .map_or(sync_threads, |value| value as u32),
            concurrency: group.concurrency.map_or(concurrency, |value| value as u32),
            scaling: group_scaling,
            timeouts: group_timeouts,
            memory: None,
        });
    }

    // ADR 0033: two groups asking the same of the same path cannot be
    // ordered, so they are refused; groups asking different things of a
    // path are ranked by how much they ask.

    for (index, group) in resolved_groups.iter().enumerate() {
        for other in &resolved_groups[index + 1..] {
            let overlap = |mine: &[String], theirs: &[String]| {
                (mine.is_empty() && theirs.is_empty())
                    || mine.iter().any(|value| theirs.contains(value))
            };
            let shared = group_prefixes(&group.paths)
                .into_iter()
                .find(|prefix| group_prefixes(&other.paths).contains(prefix));

            if let Some(path) = shared
                && overlap(&group.client_class, &other.client_class)
                && overlap(&group.host, &other.host)
                && overlap(&group.method, &other.method)
            {
                return Err(format!(
                    "worker groups {} and {} ask the same of {path}",
                    group.name, other.name
                ));
            }
        }
    }

    // Section 6.7: mounts from the declaration, else the source's own
    // table; each has a name, a path below / and an entrypoint, and no
    // two share a path.

    let mut resolved_mounts: Vec<MountSpec> = Vec::new();
    let accel_redirect = request
        .accel_redirect
        .or(table.accel_redirect)
        .or(previous.map(|previous| previous.accel_redirect))
        .unwrap_or(false);
    let cgi_local_redirect = request
        .cgi_local_redirect
        .or(table.cgi_local_redirect)
        .or(previous.map(|previous| previous.cgi_local_redirect))
        .unwrap_or(false);

    // Section 4.9: the file hint keys, from the request, else the table,
    // else the previous release.

    let sendfile_roots = if !request.sendfile_roots.is_empty() {
        request.sendfile_roots.clone()
    } else if !table.sendfile_roots.is_empty() {
        table.sendfile_roots.clone()
    } else {
        previous
            .map(|previous| previous.sendfile_roots.clone())
            .unwrap_or_default()
    };
    let sendfile_header = request
        .sendfile_header
        .clone()
        .or_else(|| table.sendfile_header.clone())
        .or_else(|| previous.and_then(|previous| previous.sendfile_header.clone()));
    muru_config::check_sendfile(&sendfile_roots, sendfile_header.as_deref())?;

    // Section 6.13: the origins gRPC-Web answers with CORS, likewise.

    let grpc_web_origins = if !request.grpc_web_origins.is_empty() {
        request.grpc_web_origins.clone()
    } else if !table.grpc_web_origins.is_empty() {
        table.grpc_web_origins.clone()
    } else {
        previous
            .map(|previous| previous.grpc_web_origins.clone())
            .unwrap_or_default()
    };
    muru_config::check_origins(&grpc_web_origins)?;

    for (name, mount) in mounts.unwrap_or(&table.mounts) {
        if !crate::model::valid_name(name) {
            return Err(format!("{name:?} is not a valid mount name"));
        }

        // Section 6.13: a gRPC mount is reached by service name, not path.

        let grpc = mount.interface == Some(muru_config::Interface::Grpc);

        if grpc && !mount.path.is_empty() {
            return Err(format!(
                "mount {name}: a gRPC mount has no path; its calls are matched by service name"
            ));
        }

        if !grpc && (!mount.path.starts_with('/') || mount.path == "/") {
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

        if !grpc && resolved_mounts.iter().any(|other| other.path == path) {
            return Err(format!("mount {name}: another mount has the path {path}"));
        }

        let mount_roots = mount
            .sendfile_roots
            .clone()
            .unwrap_or_else(|| sendfile_roots.clone());
        let mount_header = mount
            .sendfile_header
            .clone()
            .or_else(|| sendfile_header.clone());
        muru_config::check_sendfile(&mount_roots, mount_header.as_deref())
            .map_err(|error| format!("mount {name}: {error}"))?;
        muru_config::check_mount_limits(mount.concurrency, mount.max_connections, mount.interface)
            .map_err(|error| format!("mount {name}: {error}"))?;
        muru_config::check_mount_threads(mount.threads, mount.interface)
            .map_err(|error| format!("mount {name}: {error}"))?;

        // Section 4.10: a mount's redirect settings, with the release's
        // as the defaults.

        resolved_mounts.push(MountSpec {
            name: name.clone(),
            path,
            entrypoint: mount.entrypoint.clone(),
            interface: mount.interface.unwrap_or(muru_config::Interface::Auto),
            internal: mount.internal.unwrap_or(false),
            accept_redirects_from: mount.accept_redirects_from.clone(),
            accel_redirect: mount.accel_redirect.unwrap_or(accel_redirect),
            cgi_local_redirect: mount.cgi_local_redirect.unwrap_or(cgi_local_redirect),
            sendfile_roots: mount_roots,
            sendfile_header: mount_header,
            threads: mount.threads,
            concurrency: mount.concurrency.map(|value| value as u32),
            max_connections: mount.max_connections.map(|value| value as u32),
        });
    }

    // ADR 0033: a group's workers serve the mounts it lists and the
    // default mount; its paths must lie within them, so a request it
    // claims never reaches a worker without the request's mount.

    for group in &resolved_groups {
        if group.mounts.is_empty() {
            continue;
        }

        for name in &group.mounts {
            if !resolved_mounts.iter().any(|mount| &mount.name == name) {
                return Err(format!(
                    "worker group {}: no mount named {name}",
                    group.name
                ));
            }
        }

        for mount in &resolved_mounts {
            if group.mounts.contains(&mount.name) || mount.path.is_empty() {
                continue;
            }

            for prefix in group_prefixes(&group.paths) {
                let under = |path: &str, prefix: &str| {
                    prefix == "/" || path == prefix || path.starts_with(&format!("{prefix}/"))
                };

                if under(&mount.path, &prefix) || under(&prefix, &mount.path) {
                    return Err(format!(
                        "worker group {}: its path {prefix} reaches mount {} at {}, which it does not serve",
                        group.name, mount.name, mount.path
                    ));
                }
            }
        }
    }

    // Section 4.9: the release's own directories of files, by path: the
    // previous release's, then the table's, then the request's; each
    // directory resolved under the source and checked to exist.

    let static_dirs = resolve_static_dirs(
        previous.map_or(&[][..], |previous| &previous.static_dirs),
        static_dirs.unwrap_or(&table.static_dirs),
        &request.static_dirs,
        &source,
    )?;

    // Section 7.6 and 7.7: the host's own timeouts and recycling, from
    // the request, else the table, else the previous release, else the
    // defaults of appendix C.

    let carried = previous.map(|previous| &previous.timeouts);

    let table_timeouts = table.timeouts.clone().unwrap_or_default();
    let duration_ms =
        |given: Option<u64>, text: Option<&String>, name: &str| -> Result<Option<u64>, String> {
            match (given, text) {
                (Some(ms), _) => Ok(Some(ms)),
                (None, Some(text)) => muru_config::parse_duration(text)
                    .map(|duration| Some(duration.as_millis() as u64))
                    .map_err(|error| format!("{name}: {error}")),
                (None, None) => Ok(None),
            }
        };
    let max_lifetime_ms = duration_ms(
        request.max_lifetime_ms,
        table.max_lifetime.as_ref(),
        "max_lifetime",
    )?
    .or(previous.and_then(|previous| previous.max_lifetime_ms));
    let timeouts = crate::model::WorkerTimeouts {
        soft_timeout_ms: duration_ms(
            request.soft_timeout_ms,
            table_timeouts.soft_timeout.as_ref(),
            "soft_timeout",
        )?
        .or(carried.and_then(|carried| carried.soft_timeout_ms)),
        hard_timeout_ms: duration_ms(
            request.hard_timeout_ms,
            table_timeouts.hard_timeout.as_ref(),
            "hard_timeout",
        )?
        .or(carried.and_then(|carried| carried.hard_timeout_ms)),
        hard_drain_grace_ms: duration_ms(
            request.hard_drain_grace_ms,
            table_timeouts.hard_drain_grace.as_ref(),
            "hard_drain_grace",
        )?
        .or(carried.map(|carried| carried.hard_drain_grace_ms))
        .unwrap_or_else(|| crate::model::WorkerTimeouts::default().hard_drain_grace_ms),
        shutdown_timeout_ms: duration_ms(
            request.shutdown_timeout_ms,
            table_timeouts.shutdown_timeout.as_ref(),
            "shutdown_timeout",
        )?
        .or(carried.map(|carried| carried.shutdown_timeout_ms))
        .unwrap_or_else(|| crate::model::WorkerTimeouts::default().shutdown_timeout_ms),
        websocket_drain_grace_ms: duration_ms(
            request.websocket_drain_grace_ms,
            table_timeouts.websocket_drain_grace.as_ref(),
            "websocket_drain_grace",
        )?
        .or(carried.map(|carried| carried.websocket_drain_grace_ms))
        .unwrap_or_default(),
        loop_stall_timeout_ms: duration_ms(
            request.loop_stall_timeout_ms,
            table_timeouts.loop_stall_timeout.as_ref(),
            "loop_stall_timeout",
        )?
        .or(carried.map(|carried| carried.loop_stall_timeout_ms))
        .unwrap_or_else(|| crate::model::WorkerTimeouts::default().loop_stall_timeout_ms),
        gil_stall_timeout_ms: duration_ms(
            request.gil_stall_timeout_ms,
            table_timeouts.gil_stall_timeout.as_ref(),
            "gil_stall_timeout",
        )?
        .or(carried.map(|carried| carried.gil_stall_timeout_ms))
        .unwrap_or_else(|| crate::model::WorkerTimeouts::default().gil_stall_timeout_ms),
    };

    if max_lifetime_ms == Some(0) {
        return Err("max_lifetime must be longer than nothing".to_owned());
    }

    check_timeouts(&timeouts)?;

    // Section 7.7: the memory policy, the same way.

    let table_memory = table.memory.clone().unwrap_or_default();
    let size =
        |given: Option<u64>, text: Option<&String>, name: &str| -> Result<Option<u64>, String> {
            match (given, text) {
                (Some(bytes), _) => Ok(Some(bytes)),
                (None, Some(text)) => muru_config::parse_size(text)
                    .map(Some)
                    .map_err(|error| format!("memory.{name}: {error}")),
                (None, None) => Ok(None),
            }
        };
    let defaults = crate::model::MemoryPolicy::default();
    let kept = previous.map(|previous| &previous.memory);
    let memory = crate::model::MemoryPolicy {
        measure: match request.memory_measure.as_str() {
            "" => table_memory
                .measure
                .or(kept.map(|kept| kept.measure))
                .unwrap_or(defaults.measure),
            given => given
                .parse()
                .map_err(|error: muru_config::ParseError| format!("memory.measure: {error}"))?,
        },
        soft_limit: size(
            request.memory_soft_limit,
            table_memory.soft_limit.as_ref(),
            "soft_limit",
        )?
        .or(kept.and_then(|kept| kept.soft_limit)),
        soft_for_ms: duration_ms(
            request.memory_soft_for_ms,
            table_memory.soft_for.as_ref(),
            "memory.soft_for",
        )?
        .or(kept.map(|kept| kept.soft_for_ms))
        .unwrap_or(defaults.soft_for_ms),
        reclaim_first: request
            .memory_reclaim_first
            .or(table_memory.reclaim_first)
            .or(kept.map(|kept| kept.reclaim_first))
            .unwrap_or(defaults.reclaim_first),
        hard_limit: size(
            request.memory_hard_limit,
            table_memory.hard_limit.as_ref(),
            "hard_limit",
        )?
        .or(kept.and_then(|kept| kept.hard_limit)),
        check_interval_ms: duration_ms(
            request.memory_check_interval_ms,
            table_memory.check_interval.as_ref(),
            "memory.check_interval",
        )?
        .or(kept.map(|kept| kept.check_interval_ms))
        .unwrap_or(defaults.check_interval_ms),
    };

    if let (Some(soft), Some(hard)) = (memory.soft_limit, memory.hard_limit)
        && hard < soft
    {
        return Err("memory.hard_limit must not be below memory.soft_limit".to_owned());
    }

    if memory.check_interval_ms == 0 {
        return Err("memory.check_interval must be longer than nothing".to_owned());
    }

    // Section 7.1: each group's timeouts hold the same rules over the
    // release's, and its memory table is resolved over the release's.

    for group in &mut resolved_groups {
        check_timeouts(&group.timeouts.resolve(&timeouts))
            .map_err(|error| format!("worker group {}: {error}", group.name))?;

        if let Some(table) = groups
            .unwrap_or(&table.groups)
            .get(&group.name)
            .and_then(|declared| declared.memory.as_ref())
        {
            group.memory = Some(
                memory
                    .with_table(table)
                    .map_err(|error| format!("worker group {}: {error}", group.name))?,
            );
        }
    }

    let env = resolve_env_specs(
        previous.map_or(&[][..], |previous| &previous.env),
        &table.env,
        &request.env,
    )?;

    if request.max_requests == Some(0)
        || (request.max_requests.is_none() && table.max_requests == Some(0))
    {
        return Err("max_requests must be at least 1".to_owned());
    }

    // Sections 7.2 and 7.4: the default group's scaling, from the
    // declaration, else the source's own table.

    let (scaling, processes) =
        resolve_scaling(scaling.or(table.scaling.as_ref()), processes, "scaling")?;

    if scaling.host_idle_timeout_ms.is_some()
        && let Some(group) = resolved_groups
            .iter()
            .find(|group: &&GroupSpec| group.scaling.is_static() || group.scaling.min_workers > 0)
    {
        return Err(format!(
            "scaling.host_idle_timeout needs every worker group to scale to zero, and worker group {} keeps workers",
            group.name
        ));
    }

    let (error_pages, error_override, maintenance) = resolve_error_tables(
        tables,
        &table.error_pages,
        table.error_override.as_ref(),
        table.maintenance.as_ref(),
        &source,
    )?;

    let layers = resolve_layers(request, &table, previous, tables.declared, &source);

    Ok(ReleaseSpec {
        source,
        entrypoint,
        interface: match request.interface.as_str() {
            "" => table
                .interface
                .or(previous.map(|previous| previous.interface))
                .unwrap_or(muru_config::Interface::Auto),
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
            "" => table
                .preload
                .or(previous.map(|previous| previous.preload))
                .unwrap_or(muru_config::Preload::Auto),
            given => given
                .parse()
                .map_err(|error: muru_config::ParseError| error.to_string())?,
        },
        hooks: (!request.hooks.is_empty())
            .then(|| request.hooks.clone())
            .or(table.hooks)
            .or_else(|| previous.and_then(|previous| previous.hooks.clone())),
        factory,
        reflection: request
            .reflection
            .or(table.reflection)
            .or(previous.map(|previous| previous.reflection))
            .unwrap_or(false),
        grpc_web_origins,
        groups: resolved_groups,
        mounts: resolved_mounts,
        static_dirs,
        accel_redirect,
        cgi_local_redirect,
        sendfile_roots,
        sendfile_header,
        logging: logging
            .or(table.logging.as_ref())
            .and_then(|logging| logging.python.clone())
            .unwrap_or_default(),
        max_requests: request
            .max_requests
            .or(table.max_requests)
            .or(previous.and_then(|previous| previous.max_requests)),
        max_lifetime_ms,
        timeouts,
        memory,
        env,
        scaling,
        error_pages,
        error_override,
        maintenance,
        http: None,
        layers,
        interpreter,
        build,
        bytecode,
    })
}

/// Section 8.2: the build settings for a release, and whether the daemon
/// builds an environment for it.
fn resolve_build(
    request: &pb::DeployRequest,
    table: &muru_config::ToolMuru,
    source: &Path,
    provisioning: &crate::provision::Provisioning,
) -> Result<crate::model::BuildSpec, String> {
    let settings = table.build.clone().unwrap_or_default();

    let when = if request.build_when.is_empty() {
        settings.when.unwrap_or_default()
    } else {
        request
            .build_when
            .parse()
            .map_err(|error: String| format!("--build {error}"))?
    };
    let lock = muru_provision::build::lock_file(source).is_some();
    let venv = source.join(".venv").join("bin").join("python").is_file();

    let builds = match when {
        muru_config::BuildWhen::Never => false,
        muru_config::BuildWhen::Always => lock,
        muru_config::BuildWhen::Auto => lock && !venv,
    };

    if builds && provisioning.active && provisioning.uv.is_none() {
        return Err(format!(
            "{} has a uv.lock and no .venv, so the release needs an environment built, and uv was              not found (tools.uv, MURU_UV, beside muructl, PATH); install uv, or deploy a source              with its environment built",
            source.display()
        ));
    }

    let mut extras = settings.extras.clone();
    extras.extend(request.build_extras.iter().cloned());
    let mut groups = settings.groups.clone();
    groups.extend(request.build_groups.iter().cloned());

    Ok(crate::model::BuildSpec {
        when,
        extras,
        groups,
        source_builds: settings.source_builds.unwrap_or(false),
        compile_bytecode: settings.compile_bytecode.unwrap_or(true),
        builds,
    })
}

/// Section 8.1: the release's interpreter and what the probe found. With
/// provisioning off, the order before probing existed: the request's
/// value as a path, the source's .venv, the previous release's, python3.
fn resolve_interpreter(
    request: &pb::DeployRequest,
    table: &muru_config::ToolMuru,
    previous: Option<&ReleaseSpec>,
    source: &Path,
    provisioning: &crate::provision::Provisioning,
    builds: bool,
) -> Result<(PathBuf, Option<crate::model::Interpreter>), String> {
    if !provisioning.active {
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

        return Ok((python, None));
    }

    let inputs = muru_provision::Inputs {
        mode: muru_provision::Mode::Daemon,
        flag: (!request.python.is_empty()).then_some(request.python.as_str()),
        table: table.python.as_deref(),
        project: source,
        previous: previous.map(|previous| previous.python.as_path()),
        always: builds,
        uv: provisioning.uv.as_ref(),
        isolation: &provisioning.isolation,
    };

    // A request the node cannot satisfy is installed when downloads are
    // allowed (section 8.1), then resolved again; otherwise it fails
    // naming the install command.

    let resolved = match muru_provision::resolve(&inputs) {
        Ok(resolved) => resolved,
        Err(muru_provision::ResolveError::NotFound { request, .. })
            if provisioning.downloads_allowed() && provisioning.uv.is_some() =>
        {
            let uv = provisioning.uv.as_ref().expect("uv is present");
            uv.python_install(&request, &provisioning.isolation)
                .map_err(|error| format!("installing Python {request}: {error}"))?;
            muru_provision::resolve(&inputs).map_err(|error| error.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };

    let interpreter = crate::model::Interpreter::from_resolved(&resolved);
    Ok((resolved.python, Some(interpreter)))
}

/// Section 3.3: where each of a release's settings comes from, by the
/// keys `release_settings` shows.
fn resolve_layers(
    request: &pb::DeployRequest,
    table: &muru_config::ToolMuru,
    previous: Option<&ReleaseSpec>,
    declared: bool,
    source: &Path,
) -> crate::model::Layers {
    let timeouts = table.timeouts.clone().unwrap_or_default();
    let memory = table.memory.clone().unwrap_or_default();
    let before = previous.is_some();
    let venv = source.join(".venv").join("bin").join("python").is_file();

    let python = if request.python.is_empty() && venv {
        "the source's .venv"
    } else if request.python.is_empty()
        && (table.python.is_some() || source.join(".python-version").is_file())
    {
        "the project's Python request"
    } else {
        layer(!request.python.is_empty(), false, before, declared)
    };

    let keys = [
        (
            "entrypoint",
            layer(
                !request.entrypoint.is_empty(),
                table.entrypoint.is_some(),
                before,
                declared,
            ),
        ),
        ("python", python),
        (
            "interface",
            layer(
                !request.interface.is_empty(),
                table.interface.is_some(),
                before,
                declared,
            ),
        ),
        (
            "preload",
            layer(
                !request.preload.is_empty(),
                table.preload.is_some(),
                before,
                declared,
            ),
        ),
        (
            "processes",
            layer(
                request.processes.is_some(),
                table.processes.is_some(),
                before,
                declared,
            ),
        ),
        (
            "threads",
            layer(
                request.threads.is_some(),
                table.threads.is_some(),
                before,
                declared,
            ),
        ),
        (
            "sync_threads",
            layer(
                request.sync_threads.is_some(),
                table.sync_threads.is_some(),
                before,
                declared,
            ),
        ),
        (
            "concurrency",
            layer(
                request.concurrency.is_some(),
                table.concurrency.is_some(),
                before,
                declared,
            ),
        ),
        (
            "max_requests",
            layer(
                request.max_requests.is_some(),
                table.max_requests.is_some(),
                before,
                declared,
            ),
        ),
        (
            "max_lifetime",
            layer(
                request.max_lifetime_ms.is_some(),
                table.max_lifetime.is_some(),
                before,
                declared,
            ),
        ),
        (
            "timeouts.soft_timeout",
            layer(
                request.soft_timeout_ms.is_some(),
                timeouts.soft_timeout.is_some(),
                before,
                declared,
            ),
        ),
        (
            "timeouts.hard_timeout",
            layer(
                request.hard_timeout_ms.is_some(),
                timeouts.hard_timeout.is_some(),
                before,
                declared,
            ),
        ),
        (
            "timeouts.hard_drain_grace",
            layer(
                request.hard_drain_grace_ms.is_some(),
                timeouts.hard_drain_grace.is_some(),
                before,
                declared,
            ),
        ),
        (
            "timeouts.websocket_drain_grace",
            layer(
                request.websocket_drain_grace_ms.is_some(),
                timeouts.websocket_drain_grace.is_some(),
                before,
                declared,
            ),
        ),
        (
            "timeouts.shutdown_timeout",
            layer(
                request.shutdown_timeout_ms.is_some(),
                timeouts.shutdown_timeout.is_some(),
                before,
                declared,
            ),
        ),
        (
            "timeouts.loop_stall_timeout",
            layer(
                request.loop_stall_timeout_ms.is_some(),
                timeouts.loop_stall_timeout.is_some(),
                before,
                declared,
            ),
        ),
        (
            "timeouts.gil_stall_timeout",
            layer(
                request.gil_stall_timeout_ms.is_some(),
                timeouts.gil_stall_timeout.is_some(),
                before,
                declared,
            ),
        ),
        (
            "memory.measure",
            layer(
                !request.memory_measure.is_empty(),
                memory.measure.is_some(),
                before,
                declared,
            ),
        ),
        (
            "memory.soft_limit",
            layer(
                request.memory_soft_limit.is_some(),
                memory.soft_limit.is_some(),
                before,
                declared,
            ),
        ),
        (
            "memory.soft_for",
            layer(
                request.memory_soft_for_ms.is_some(),
                memory.soft_for.is_some(),
                before,
                declared,
            ),
        ),
        (
            "memory.reclaim_first",
            layer(
                request.memory_reclaim_first.is_some(),
                memory.reclaim_first.is_some(),
                before,
                declared,
            ),
        ),
        (
            "memory.hard_limit",
            layer(
                request.memory_hard_limit.is_some(),
                memory.hard_limit.is_some(),
                before,
                declared,
            ),
        ),
        (
            "memory.check_interval",
            layer(
                request.memory_check_interval_ms.is_some(),
                memory.check_interval.is_some(),
                before,
                declared,
            ),
        ),
        (
            "accel_redirect",
            layer(
                request.accel_redirect.is_some(),
                table.accel_redirect.is_some(),
                before,
                declared,
            ),
        ),
        (
            "cgi_local_redirect",
            layer(
                request.cgi_local_redirect.is_some(),
                table.cgi_local_redirect.is_some(),
                before,
                declared,
            ),
        ),
    ];

    crate::model::Layers(
        keys.into_iter()
            .map(|(key, layer)| (key.to_owned(), layer.to_owned()))
            .collect(),
    )
}

/// Section 3.3: a release's settings as `config show --effective`
/// shows them beside their layers, by key.
fn release_settings(spec: &ReleaseSpec) -> Vec<(&'static str, String)> {
    let optional = |value: Option<u64>, format: fn(u64) -> String| {
        value.map_or_else(|| "off".to_owned(), format)
    };

    vec![
        ("entrypoint", spec.entrypoint.clone()),
        ("python", spec.python.display().to_string()),
        ("build.when", spec.build.when.as_str().to_owned()),
        ("bytecode", spec.bytecode.describe()),
        ("interface", spec.interface.to_string()),
        ("preload", spec.preload.to_string()),
        ("processes", spec.processes.to_string()),
        ("threads", spec.threads.to_string()),
        ("sync_threads", spec.sync_threads.to_string()),
        ("concurrency", spec.concurrency.to_string()),
        (
            "max_requests",
            optional(spec.max_requests, |value| value.to_string()),
        ),
        ("max_lifetime", optional(spec.max_lifetime_ms, format_ms)),
        (
            "timeouts.soft_timeout",
            optional(spec.timeouts.soft_timeout_ms, format_ms),
        ),
        (
            "timeouts.hard_timeout",
            optional(spec.timeouts.hard_timeout_ms, format_ms),
        ),
        (
            "timeouts.hard_drain_grace",
            format_ms(spec.timeouts.hard_drain_grace_ms),
        ),
        (
            "timeouts.shutdown_timeout",
            format_ms(spec.timeouts.shutdown_timeout_ms),
        ),
        (
            "timeouts.websocket_drain_grace",
            format_ms(spec.timeouts.websocket_drain_grace_ms),
        ),
        (
            "timeouts.loop_stall_timeout",
            format_ms(spec.timeouts.loop_stall_timeout_ms),
        ),
        (
            "timeouts.gil_stall_timeout",
            format_ms(spec.timeouts.gil_stall_timeout_ms),
        ),
        ("memory.measure", spec.memory.measure.to_string()),
        (
            "memory.soft_limit",
            optional(spec.memory.soft_limit, muru_config::format_size),
        ),
        ("memory.soft_for", format_ms(spec.memory.soft_for_ms)),
        (
            "memory.reclaim_first",
            spec.memory.reclaim_first.to_string(),
        ),
        (
            "memory.hard_limit",
            optional(spec.memory.hard_limit, muru_config::format_size),
        ),
        (
            "memory.check_interval",
            format_ms(spec.memory.check_interval_ms),
        ),
        ("accel_redirect", spec.accel_redirect.to_string()),
        ("cgi_local_redirect", spec.cgi_local_redirect.to_string()),
    ]
}

/// Section 3.3: each setting of the releases an exported document
/// declares, with the layer it came from, one line a setting, for
/// `config show --effective`.
pub fn release_layers(store: &Store, document: &Document) -> Result<Vec<String>, StoreError> {
    let mut lines = Vec::new();

    for (app, table) in &document.app {
        for id in table.release.keys() {
            let release = store.release(app, id)?;

            if release.spec.http.is_some() {
                continue;
            }

            if release.spec.layers.is_empty() {
                lines.push(format!(
                    "{app}@{id}: made before Muru recorded where settings come from"
                ));
                continue;
            }

            for (key, value) in release_settings(&release.spec) {
                let layer = release
                    .spec
                    .layers
                    .0
                    .get(key)
                    .map_or("not recorded", String::as_str);
                lines.push(format!("{app}@{id}: {key} = {value} ({layer})"));
            }
        }
    }

    Ok(lines)
}

/// Section 4.9: the release's own directories of files, layered by
/// path prefix: the previous release's, the table's, then the request's,
/// each directory taken under the source when relative and checked to
/// be one.
fn resolve_static_dirs(
    previous: &[StaticSpec],
    table: &[muru_config::StaticDir],
    request: &[pb::StaticDir],
    source: &Path,
) -> Result<Vec<StaticSpec>, String> {
    muru_config::check_static(table)?;
    let mut by_path: BTreeMap<String, StaticSpec> = previous
        .iter()
        .map(|entry| (prefix_key(&entry.path), entry.clone()))
        .collect();

    for entry in table {
        by_path.insert(prefix_key(&entry.path), static_spec(entry, source)?);
    }

    for entry in request {
        let table_entry = muru_config::StaticDir {
            path: entry.path.clone(),
            dir: entry.dir.clone(),
            ..Default::default()
        };
        muru_config::check_static(std::slice::from_ref(&table_entry))?;
        by_path.insert(prefix_key(&entry.path), static_spec(&table_entry, source)?);
    }

    Ok(by_path.into_values().collect())
}

/// A prefix as paths are matched by it, without a trailing slash.
fn prefix_key(path: &str) -> String {
    muru_edge::routing::group_prefix(path)
}

/// One of the release's own directories, resolved and checked.
fn static_spec(entry: &muru_config::StaticDir, source: &Path) -> Result<StaticSpec, String> {
    let dir = if Path::new(&entry.dir).is_absolute() {
        PathBuf::from(&entry.dir)
    } else {
        source.join(&entry.dir)
    };

    if !dir.is_dir() {
        return Err(format!(
            "static {}: {} is not a directory",
            entry.path,
            dir.display()
        ));
    }

    let cache = entry.cache.as_ref();
    Ok(StaticSpec {
        path: entry.path.clone(),
        dir,
        index: entry.index.clone(),
        fallback_file: entry.fallback.clone(),
        cache_max_age_s: cache
            .and_then(|cache| cache.max_age.as_deref())
            .map(|text| {
                muru_config::parse_duration(text)
                    .map(|duration| duration.as_secs())
                    .map_err(|error| format!("static {}: cache.max_age: {error}", entry.path))
            })
            .transpose()?,
        cache_immutable: cache.and_then(|cache| cache.immutable).unwrap_or(false),
        cache_except: cache.map(|cache| cache.except.clone()).unwrap_or_default(),
        redirect_slash: entry.redirect_slash.unwrap_or(true),
        case_insensitive: entry.case.as_deref() == Some("insensitive"),
    })
}

/// Section 10.8: the environment, the table's entries with the
/// request's over them, references kept as references.
fn resolve_env_specs(
    previous: &[EnvSpec],
    table: &BTreeMap<String, muru_config::EnvValue>,
    request: &[pb::EnvEntry],
) -> Result<Vec<EnvSpec>, String> {
    let mut env_table: BTreeMap<String, EnvSpec> = previous
        .iter()
        .map(|spec| (spec.name.clone(), spec.clone()))
        .collect();

    for (name, value) in table {
        env_table.insert(name.clone(), env_spec_from_table(name, value)?);
    }

    for entry in request {
        if !crate::model::valid_env_name(&entry.name) {
            return Err(format!("env: {:?} is not a variable name", entry.name));
        }

        if entry.secret_store.is_empty() != entry.secret_key.is_empty() {
            return Err(format!(
                "env {}: a secret needs both a store and a key",
                entry.name
            ));
        }

        env_table.insert(
            entry.name.clone(),
            EnvSpec {
                name: entry.name.clone(),
                value: entry.secret_store.is_empty().then(|| entry.value.clone()),
                secret_store: (!entry.secret_store.is_empty()).then(|| entry.secret_store.clone()),
                secret_key: (!entry.secret_key.is_empty()).then(|| entry.secret_key.clone()),
                as_file: entry.as_file && !entry.secret_store.is_empty(),
            },
        );
    }

    Ok(env_table.into_values().collect())
}

/// Section 4.12: the release's error pages, override and maintenance
/// exemptions, from the declared tables else the fallbacks (the
/// source's own [tool.muru] table); a page file is checked to exist
/// under the source.
type ErrorTables = (
    BTreeMap<String, muru_config::ErrorPage>,
    Option<muru_config::ErrorOverride>,
    Option<muru_config::Maintenance>,
);

fn resolve_error_tables(
    tables: &Tables<'_>,
    fallback_pages: &BTreeMap<String, muru_config::ErrorPage>,
    fallback_override: Option<&muru_config::ErrorOverride>,
    fallback_maintenance: Option<&muru_config::Maintenance>,
    source: &Path,
) -> Result<ErrorTables, String> {
    let error_pages = tables
        .error_pages
        .cloned()
        .unwrap_or_else(|| fallback_pages.clone());
    muru_config::check_error_pages(&error_pages, true)?;

    for (key, page) in &error_pages {
        if let Some(file) = &page.file
            && !source.join(file).is_file()
        {
            return Err(format!(
                "error_pages: {key}: {} is not a file",
                source.join(file).display()
            ));
        }
    }

    let error_override = tables
        .error_override
        .cloned()
        .or_else(|| fallback_override.cloned());

    if let Some(override_) = &error_override {
        muru_config::check_error_override(override_)?;
    }

    let maintenance = tables
        .maintenance
        .cloned()
        .or_else(|| fallback_maintenance.cloned());

    if let Some(maintenance) = &maintenance {
        muru_config::check_maintenance(maintenance)?;
    }

    Ok((error_pages, error_override, maintenance))
}

/// A deploy request's keys as the release table they amount to, for the
/// kind check of section 9.9: which keys are set is what matters.
fn kind_table_from(request: &pb::DeployRequest) -> Result<muru_config::Release, String> {
    let readiness = (!request.readiness_path.is_empty() || request.readiness_timeout_ms.is_some())
        .then(|| muru_config::Readiness {
            path: (!request.readiness_path.is_empty()).then(|| request.readiness_path.clone()),
            timeout: request.readiness_timeout_ms.map(format_ms),
        });
    let timeouts_given = request.soft_timeout_ms.is_some()
        || request.hard_timeout_ms.is_some()
        || request.hard_drain_grace_ms.is_some()
        || request.shutdown_timeout_ms.is_some()
        || request.websocket_drain_grace_ms.is_some()
        || request.loop_stall_timeout_ms.is_some()
        || request.gil_stall_timeout_ms.is_some();
    let memory_given = !request.memory_measure.is_empty()
        || request.memory_soft_limit.is_some()
        || request.memory_soft_for_ms.is_some()
        || request.memory_reclaim_first.is_some()
        || request.memory_hard_limit.is_some()
        || request.memory_check_interval_ms.is_some();

    Ok(muru_config::Release {
        static_dirs: request
            .static_dirs
            .iter()
            .map(|entry| muru_config::StaticDir {
                path: entry.path.clone(),
                dir: entry.dir.clone(),
                ..Default::default()
            })
            .collect(),
        kind: parse_optional(&request.kind)?,
        profile: parse_optional(&request.profile)?,
        command: request.command.clone(),
        cwd: None,
        bind: parse_optional(&request.bind)?,
        instances: request.instances,
        readiness,
        stop_signal: (!request.stop_signal.is_empty()).then(|| request.stop_signal.clone()),
        source: Some(request.source.clone()),
        entrypoint: request.entrypoint.clone(),
        interface: parse_optional(&request.interface)?,
        reflection: request.reflection,
        grpc_web_origins: request.grpc_web_origins.clone(),
        factory: request.factory,
        factory_phase: parse_optional(&request.factory_phase)?,
        python: (!request.python.is_empty()).then(|| request.python.clone()),
        build: None,
        bytecode: None,
        processes: request.processes.map(|value| value as usize),
        threads: request.threads.map(|value| value as usize),
        sync_threads: request.sync_threads.map(|value| value as usize),
        concurrency: request.concurrency.map(|value| value as usize),
        scaling: None,
        preload: parse_optional(&request.preload)?,
        hooks: (!request.hooks.is_empty()).then(|| request.hooks.clone()),
        groups: BTreeMap::new(),
        mounts: BTreeMap::new(),
        logging: None,
        max_requests: request.max_requests,
        max_lifetime: request.max_lifetime_ms.map(format_ms),
        timeouts: timeouts_given.then(|| muru_config::Timeouts {
            soft_timeout: request.soft_timeout_ms.map(format_ms),
            hard_timeout: request.hard_timeout_ms.map(format_ms),
            hard_drain_grace: request.hard_drain_grace_ms.map(format_ms),
            shutdown_timeout: request.shutdown_timeout_ms.map(format_ms),
            websocket_drain_grace: request.websocket_drain_grace_ms.map(format_ms),
            loop_stall_timeout: request.loop_stall_timeout_ms.map(format_ms),
            gil_stall_timeout: request.gil_stall_timeout_ms.map(format_ms),
        }),
        memory: memory_given.then(muru_config::Memory::default),
        env: BTreeMap::new(),
        accel_redirect: request.accel_redirect,
        cgi_local_redirect: request.cgi_local_redirect,
        sendfile_roots: request.sendfile_roots.clone(),
        sendfile_header: request.sendfile_header.clone(),
        error_pages: BTreeMap::new(),
        error_override: None,
        maintenance: None,
    })
}

/// A setting as a request writes it: none when empty, else parsed.
fn parse_optional<T: std::str::FromStr<Err = muru_config::ParseError>>(
    text: &str,
) -> Result<Option<T>, String> {
    if text.is_empty() {
        return Ok(None);
    }

    text.parse()
        .map(Some)
        .map_err(|error: muru_config::ParseError| error.to_string())
}

/// Milliseconds as a duration a table writes.
fn format_ms(ms: u64) -> String {
    muru_config::format_duration(Duration::from_millis(ms))
}

/// Section 9.9: a managed HTTP release's spec from a request already
/// checked against its kind: the command, the server, how it binds and
/// is probed, the environment and the error tables, with the worker
/// settings left at their defaults, unused.
fn resolve_http_spec(
    request: &pb::DeployRequest,
    table: &muru_config::Release,
    source: PathBuf,
    tables: &Tables<'_>,
) -> Result<ReleaseSpec, String> {
    let readiness_timeout_ms = request
        .readiness_timeout_ms
        .unwrap_or(crate::servers::READINESS_TIMEOUT.as_millis() as u64);

    if readiness_timeout_ms == 0 {
        return Err("readiness.timeout must be longer than nothing".to_owned());
    }

    let shutdown_timeout_ms = request
        .shutdown_timeout_ms
        .unwrap_or_else(|| crate::model::WorkerTimeouts::default().shutdown_timeout_ms);

    if shutdown_timeout_ms == 0 {
        return Err("shutdown_timeout must be longer than nothing".to_owned());
    }

    let env = resolve_env_specs(&[], &BTreeMap::new(), &request.env)?;
    let (error_pages, error_override, maintenance) =
        resolve_error_tables(tables, &BTreeMap::new(), None, None, &source)?;
    muru_config::check_sendfile(&table.sendfile_roots, table.sendfile_header.as_deref())?;

    Ok(ReleaseSpec {
        source,
        entrypoint: String::new(),
        interface: muru_config::Interface::Auto,
        python: PathBuf::new(),
        processes: 0,
        threads: 0,
        sync_threads: 0,
        concurrency: 0,
        preload: muru_config::Preload::Auto,
        hooks: None,
        factory: None,
        reflection: false,
        grpc_web_origins: Vec::new(),
        groups: Vec::new(),
        mounts: Vec::new(),
        static_dirs: Vec::new(),
        accel_redirect: false,
        cgi_local_redirect: false,
        sendfile_roots: table.sendfile_roots.clone(),
        sendfile_header: table.sendfile_header.clone(),
        logging: muru_config::PythonLogging::default(),
        max_requests: None,
        max_lifetime_ms: None,
        timeouts: crate::model::WorkerTimeouts {
            shutdown_timeout_ms,
            ..crate::model::WorkerTimeouts::default()
        },
        memory: crate::model::MemoryPolicy::default(),
        env,
        scaling: ScalingSpec::default(),
        error_pages,
        error_override,
        maintenance,
        http: Some(HttpServerSpec {
            profile: table.profile.unwrap_or(muru_config::ServerProfile::Generic),
            command: request.command.clone(),
            bind: table.bind.unwrap_or(muru_config::ServerBind::Unix),
            instances: request.instances.unwrap_or(1).max(1),
            readiness_path: (!request.readiness_path.is_empty())
                .then(|| request.readiness_path.clone()),
            readiness_timeout_ms,
            stop_signal: table.stop_signal.clone(),
        }),
        layers: crate::model::Layers::default(),
        interpreter: None,
        build: crate::model::BuildSpec::default(),
        bytecode: crate::model::BytecodeSpec::default(),
    })
}

/// Section 7.2 and 7.4: a scaling table resolved against the defaults of
/// appendix C, with the initial worker count raised to the floor. The
/// spare and target modes need a ceiling; static mode takes no other
/// keys, so a key that does nothing is never written by mistake.
fn resolve_scaling(
    table: Option<&muru_config::Scaling>,
    processes: u32,
    what: &str,
) -> Result<(ScalingSpec, u32), String> {
    let defaults = ScalingSpec::default();
    let Some(table) = table else {
        return Ok((
            ScalingSpec {
                max_workers: processes,
                ..defaults
            },
            processes,
        ));
    };
    let mode = table.mode.unwrap_or(ScalingMode::Static);

    if mode == ScalingMode::Static {
        let only_mode = muru_config::Scaling {
            mode: table.mode,
            advisory: table.advisory,
            ..muru_config::Scaling::default()
        };

        if *table != only_mode {
            return Err(format!("{what}: mode static takes no other keys"));
        }

        return Ok((
            ScalingSpec {
                max_workers: processes,
                advisory: table.advisory.unwrap_or(false),
                ..defaults
            },
            processes,
        ));
    }

    let Some(max_workers) = table.max_workers else {
        return Err(format!("{what}: mode {} needs max_workers", mode.as_str()));
    };
    let min_workers = table.min_workers.unwrap_or(1);

    if max_workers < min_workers {
        return Err(format!(
            "{what}: max_workers {max_workers} is below min_workers {min_workers}"
        ));
    }

    if processes as usize > max_workers {
        return Err(format!(
            "{what}: processes {processes} is over max_workers {max_workers}"
        ));
    }

    let min_spare = table.min_spare.unwrap_or(1);
    let max_spare = table.max_spare.unwrap_or((max_workers / 4).max(3));

    if mode == ScalingMode::Spare && max_spare < min_spare + 2 {
        return Err(format!(
            "{what}: max_spare must be at least min_spare + 2, so {}",
            min_spare + 2
        ));
    }

    let slot_utilization = table.slot_utilization.unwrap_or(0.7);

    if !(slot_utilization > 0.0 && slot_utilization <= 1.0) {
        return Err(format!(
            "{what}: slot_utilization must be over 0 and at most 1"
        ));
    }

    let duration = |name: &str, text: Option<&String>, default: u64| -> Result<u64, String> {
        match text {
            Some(text) => muru_config::parse_duration(text)
                .map(|value| value.as_millis() as u64)
                .map_err(|error| format!("{what}.{name}: {error}")),
            None => Ok(default),
        }
    };
    let max_step_up = table.max_step_up.unwrap_or(4);
    let churn_budget = table.churn_budget.unwrap_or(6);

    if max_step_up == 0 {
        return Err(format!("{what}: max_step_up must be at least 1"));
    }

    if churn_budget == 0 {
        return Err(format!("{what}: churn_budget must be at least 1"));
    }

    let spec = ScalingSpec {
        mode,
        min_workers: min_workers as u32,
        max_workers: max_workers as u32,
        min_spare: min_spare as u32,
        max_spare: max_spare as u32,
        slot_utilization_percent: (slot_utilization * 100.0).round() as u32,
        queue_wait_p95_ms: match &table.queue_wait_p95 {
            Some(text) => Some(duration("queue_wait_p95", Some(text), 0)?),
            None => None,
        },
        scale_out_window_ms: duration(
            "scale_out_window",
            table.scale_out_window.as_ref(),
            defaults.scale_out_window_ms,
        )?,
        scale_in_window_ms: duration(
            "scale_in_window",
            table.scale_in_window.as_ref(),
            defaults.scale_in_window_ms,
        )?,
        hold_after_growth_ms: duration(
            "hold_after_growth",
            table.hold_after_growth.as_ref(),
            defaults.hold_after_growth_ms,
        )?,
        reap_interval_ms: duration(
            "reap_interval",
            table.reap_interval.as_ref(),
            defaults.reap_interval_ms,
        )?,
        idle_reap_after_ms: duration(
            "idle_reap_after",
            table.idle_reap_after.as_ref(),
            defaults.idle_reap_after_ms,
        )?,
        min_lifetime_ms: match &table.min_lifetime {
            Some(text) => Some(duration("min_lifetime", Some(text), 0)?),
            None => None,
        },
        max_step_up: max_step_up as u32,
        churn_budget: churn_budget as u32,
        advisory: table.advisory.unwrap_or(false),
        host_idle_timeout_ms: match &table.host_idle_timeout {
            Some(text) => Some(duration("host_idle_timeout", Some(text), 0)?),
            None => None,
        },
    };

    if spec.host_idle_timeout_ms.is_some() && spec.min_workers > 0 {
        return Err(format!(
            "{what}: host_idle_timeout needs min_workers = 0, so the group can scale to zero"
        ));
    }

    let initial = processes.max(spec.min_workers);
    Ok((spec, initial))
}

/// A resolved scaling spec as the table that declares it, every key
/// written; none for a fixed count.
fn scaling_table(spec: &ScalingSpec) -> Option<muru_config::Scaling> {
    if spec.is_static() {
        return None;
    }

    let duration = |ms: u64| Some(muru_config::format_duration(Duration::from_millis(ms)));
    Some(muru_config::Scaling {
        mode: Some(spec.mode),
        min_workers: Some(spec.min_workers as usize),
        max_workers: Some(spec.max_workers as usize),
        min_spare: (spec.mode == ScalingMode::Spare).then_some(spec.min_spare as usize),
        max_spare: (spec.mode == ScalingMode::Spare).then_some(spec.max_spare as usize),
        slot_utilization: (spec.mode == ScalingMode::Target)
            .then_some(spec.slot_utilization_percent as f64 / 100.0),
        queue_wait_p95: spec.queue_wait_p95_ms.and_then(duration),
        scale_out_window: duration(spec.scale_out_window_ms),
        scale_in_window: duration(spec.scale_in_window_ms),
        hold_after_growth: duration(spec.hold_after_growth_ms),
        reap_interval: duration(spec.reap_interval_ms),
        idle_reap_after: duration(spec.idle_reap_after_ms),
        min_lifetime: spec.min_lifetime_ms.and_then(duration),
        max_step_up: Some(spec.max_step_up as usize),
        churn_budget: Some(spec.churn_budget as usize),
        advisory: spec.advisory.then_some(true),
        host_idle_timeout: spec.host_idle_timeout_ms.and_then(duration),
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

/// One variable of an `[env]` table as the spec records it.
fn env_spec_from_table(name: &str, value: &muru_config::EnvValue) -> Result<EnvSpec, String> {
    if !crate::model::valid_env_name(name) {
        return Err(format!("env: {name:?} is not a variable name"));
    }

    Ok(match value {
        muru_config::EnvValue::Plain(text) => EnvSpec {
            name: name.to_owned(),
            value: Some(text.clone()),
            secret_store: None,
            secret_key: None,
            as_file: false,
        },
        muru_config::EnvValue::Secret(secret) => {
            if secret.secret.store.is_empty() || secret.secret.key.is_empty() {
                return Err(format!("env {name}: a secret needs a store and a key"));
            }

            let as_file = match secret.deliver.as_deref() {
                None | Some("env") => false,
                Some("file") => true,
                Some(other) => {
                    return Err(format!("env {name}: as must be file or env, not {other:?}"));
                }
            };

            EnvSpec {
                name: name.to_owned(),
                value: None,
                secret_store: Some(secret.secret.store.clone()),
                secret_key: Some(secret.secret.key.clone()),
                as_file,
            }
        }
    })
}

/// A declared release as the deploy request it stands for.
fn request_from(app: &str, table: &ReleaseTable, source: &str) -> pb::DeployRequest {
    pb::DeployRequest {
        app: app.to_owned(),
        source: source.to_owned(),
        override_freeze: false,
        fresh: false,
        static_dirs: Vec::new(),
        retain_workers: None,
        kind: table
            .kind
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        profile: table
            .profile
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        command: table.command.clone(),
        bind: table
            .bind
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        instances: table.instances,
        readiness_path: table
            .readiness
            .as_ref()
            .and_then(|readiness| readiness.path.clone())
            .unwrap_or_default(),
        readiness_timeout_ms: duration_field(
            table
                .readiness
                .as_ref()
                .and_then(|readiness| readiness.timeout.as_ref()),
        ),
        stop_signal: table.stop_signal.clone().unwrap_or_default(),
        entrypoint: table.entrypoint.clone(),
        interface: table
            .interface
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        python: table.python.clone().unwrap_or_default(),
        build_when: table
            .build
            .as_ref()
            .and_then(|build| build.when)
            .map(|when| when.as_str().to_owned())
            .unwrap_or_default(),
        build_extras: table
            .build
            .as_ref()
            .map(|build| build.extras.clone())
            .unwrap_or_default(),
        build_groups: table
            .build
            .as_ref()
            .map(|build| build.groups.clone())
            .unwrap_or_default(),
        bytecode: table
            .bytecode
            .map(|bytecode| bytecode.as_str().to_owned())
            .unwrap_or_default(),
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
        reflection: table.reflection,
        grpc_web_origins: table.grpc_web_origins.clone(),
        factory_phase: table
            .factory_phase
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        release: String::new(),
        wait: true,
        retain_ms: None,
        hold: false,
        max_requests: table.max_requests,
        max_lifetime_ms: duration_field(table.max_lifetime.as_ref()),
        accel_redirect: table.accel_redirect,
        cgi_local_redirect: table.cgi_local_redirect,
        sendfile_roots: table.sendfile_roots.clone(),
        sendfile_header: table.sendfile_header.clone(),
        soft_timeout_ms: duration_field(
            table
                .timeouts
                .as_ref()
                .and_then(|t| t.soft_timeout.as_ref()),
        ),
        hard_timeout_ms: duration_field(
            table
                .timeouts
                .as_ref()
                .and_then(|t| t.hard_timeout.as_ref()),
        ),
        hard_drain_grace_ms: duration_field(
            table
                .timeouts
                .as_ref()
                .and_then(|t| t.hard_drain_grace.as_ref()),
        ),
        shutdown_timeout_ms: duration_field(
            table
                .timeouts
                .as_ref()
                .and_then(|t| t.shutdown_timeout.as_ref()),
        ),
        websocket_drain_grace_ms: duration_field(
            table
                .timeouts
                .as_ref()
                .and_then(|t| t.websocket_drain_grace.as_ref()),
        ),
        loop_stall_timeout_ms: duration_field(
            table
                .timeouts
                .as_ref()
                .and_then(|t| t.loop_stall_timeout.as_ref()),
        ),
        gil_stall_timeout_ms: duration_field(
            table
                .timeouts
                .as_ref()
                .and_then(|t| t.gil_stall_timeout.as_ref()),
        ),
        memory_measure: table
            .memory
            .as_ref()
            .and_then(|memory| memory.measure)
            .map(|measure| measure.as_str().to_owned())
            .unwrap_or_default(),
        memory_soft_limit: size_field(table.memory.as_ref().and_then(|m| m.soft_limit.as_ref())),
        memory_soft_for_ms: duration_field(table.memory.as_ref().and_then(|m| m.soft_for.as_ref())),
        memory_reclaim_first: table.memory.as_ref().and_then(|m| m.reclaim_first),
        memory_hard_limit: size_field(table.memory.as_ref().and_then(|m| m.hard_limit.as_ref())),
        memory_check_interval_ms: duration_field(
            table
                .memory
                .as_ref()
                .and_then(|m| m.check_interval.as_ref()),
        ),
        env: table
            .env
            .iter()
            .map(|(name, value)| match value {
                muru_config::EnvValue::Plain(text) => pb::EnvEntry {
                    name: name.clone(),
                    value: text.clone(),
                    ..Default::default()
                },
                muru_config::EnvValue::Secret(secret) => pb::EnvEntry {
                    name: name.clone(),
                    secret_store: secret.secret.store.clone(),
                    secret_key: secret.secret.key.clone(),
                    as_file: secret.deliver.as_deref() == Some("file"),
                    ..Default::default()
                },
            })
            .collect(),
    }
}

/// A size a table writes, as bytes for a request; one that does not
/// parse is left out, and the validation of the declared release
/// reports it.
fn size_field(text: Option<&String>) -> Option<u64> {
    text.and_then(|text| muru_config::parse_size(text).ok())
}

/// A duration a table writes, as milliseconds for a request; one that
/// does not parse is left out, and the validation of the declared
/// release reports it.
fn duration_field(text: Option<&String>) -> Option<u64> {
    text.and_then(|text| muru_config::parse_duration(text).ok())
        .map(|duration| duration.as_millis() as u64)
}

/// A recorded spec as the table that declares it, every key written so
/// the file means the same thing without the source's [tool.muru].
fn table_from(spec: &ReleaseSpec) -> ReleaseTable {
    // Section 9.9: a managed HTTP release writes its server keys and
    // the few settings that apply to it.

    if let Some(http) = &spec.http {
        return ReleaseTable {
            kind: Some(muru_config::ReleaseKind::Http),
            profile: Some(http.profile),
            command: http.command.clone(),
            cwd: Some(spec.source.display().to_string()),
            bind: Some(http.bind),
            instances: Some(http.instances),
            readiness: Some(muru_config::Readiness {
                path: http.readiness_path.clone(),
                timeout: Some(format_ms(http.readiness_timeout_ms)),
            }),
            stop_signal: http.stop_signal.clone(),
            timeouts: Some(muru_config::Timeouts {
                shutdown_timeout: Some(format_ms(spec.timeouts.shutdown_timeout_ms)),
                ..muru_config::Timeouts::default()
            }),
            env: env_table_from(&spec.env),
            error_pages: spec.error_pages.clone(),
            error_override: spec.error_override.clone(),
            maintenance: spec.maintenance.clone(),
            ..ReleaseTable::default()
        };
    }

    ReleaseTable {
        kind: None,
        profile: None,
        command: Vec::new(),
        cwd: None,
        bind: None,
        instances: None,
        readiness: None,
        stop_signal: None,
        source: Some(spec.source.display().to_string()),
        entrypoint: spec.entrypoint.clone(),
        interface: Some(spec.interface),
        reflection: spec.reflection.then_some(true),
        grpc_web_origins: spec.grpc_web_origins.clone(),
        factory: spec.factory.map(|_| true),
        factory_phase: spec.factory,
        python: Some(spec.python.display().to_string()),
        build: None,
        bytecode: None,
        processes: Some(spec.processes as usize),
        threads: Some(spec.threads as usize),
        sync_threads: Some(spec.sync_threads as usize),
        concurrency: Some(spec.concurrency as usize),
        scaling: scaling_table(&spec.scaling),
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
                        mounts: group.mounts.clone(),
                        matches: (!group.client_class.is_empty()
                            || !group.host.is_empty()
                            || !group.method.is_empty())
                        .then(|| muru_config::GroupMatch {
                            client_class: group.client_class.clone(),
                            host: group.host.clone(),
                            method: group.method.clone(),
                        }),
                        optional: group.optional.then_some(true),
                        processes: Some(group.processes as usize),
                        threads: Some(group.threads as usize),
                        sync_threads: Some(group.sync_threads as usize),
                        concurrency: Some(group.concurrency as usize),
                        scaling: scaling_table(&group.scaling),
                        timeouts: group.timeouts.table(),
                        memory: group.memory.as_ref().map(crate::model::MemoryPolicy::table),
                    },
                )
            })
            .collect(),
        static_dirs: spec.static_dirs.iter().map(StaticSpec::table).collect(),
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
                        internal: mount.internal.then_some(true),
                        accept_redirects_from: mount.accept_redirects_from.clone(),
                        accel_redirect: (mount.accel_redirect != spec.accel_redirect)
                            .then_some(mount.accel_redirect),
                        cgi_local_redirect: (mount.cgi_local_redirect != spec.cgi_local_redirect)
                            .then_some(mount.cgi_local_redirect),
                        sendfile_roots: (mount.sendfile_roots != spec.sendfile_roots)
                            .then(|| mount.sendfile_roots.clone()),
                        sendfile_header: (mount.sendfile_header != spec.sendfile_header)
                            .then(|| mount.sendfile_header.clone())
                            .flatten(),
                        threads: mount.threads,
                        concurrency: mount.concurrency.map(|value| value as usize),
                        max_connections: mount.max_connections.map(|value| value as usize),
                    },
                )
            })
            .collect(),
        logging: (spec.logging != muru_config::PythonLogging::default()).then(|| {
            muru_config::Logging {
                python: Some(spec.logging.clone()),
            }
        }),
        max_requests: spec.max_requests,
        max_lifetime: spec
            .max_lifetime_ms
            .map(|ms| muru_config::format_duration(Duration::from_millis(ms))),
        timeouts: (spec.timeouts != crate::model::WorkerTimeouts::default()).then(|| {
            muru_config::Timeouts {
                soft_timeout: spec
                    .timeouts
                    .soft_timeout_ms
                    .map(|ms| muru_config::format_duration(Duration::from_millis(ms))),
                hard_timeout: spec
                    .timeouts
                    .hard_timeout_ms
                    .map(|ms| muru_config::format_duration(Duration::from_millis(ms))),
                hard_drain_grace: Some(muru_config::format_duration(Duration::from_millis(
                    spec.timeouts.hard_drain_grace_ms,
                ))),
                shutdown_timeout: Some(muru_config::format_duration(Duration::from_millis(
                    spec.timeouts.shutdown_timeout_ms,
                ))),
                websocket_drain_grace: Some(muru_config::format_duration(Duration::from_millis(
                    spec.timeouts.websocket_drain_grace_ms,
                ))),
                loop_stall_timeout: Some(muru_config::format_duration(Duration::from_millis(
                    spec.timeouts.loop_stall_timeout_ms,
                ))),
                gil_stall_timeout: Some(muru_config::format_duration(Duration::from_millis(
                    spec.timeouts.gil_stall_timeout_ms,
                ))),
            }
        }),
        env: env_table_from(&spec.env),
        memory: (spec.memory != crate::model::MemoryPolicy::default()).then(|| {
            muru_config::Memory {
                measure: Some(spec.memory.measure),
                soft_limit: spec.memory.soft_limit.map(muru_config::format_size),
                soft_for: Some(muru_config::format_duration(Duration::from_millis(
                    spec.memory.soft_for_ms,
                ))),
                reclaim_first: Some(spec.memory.reclaim_first),
                hard_limit: spec.memory.hard_limit.map(muru_config::format_size),
                check_interval: Some(muru_config::format_duration(Duration::from_millis(
                    spec.memory.check_interval_ms,
                ))),
            }
        }),
        accel_redirect: spec.accel_redirect.then_some(true),
        cgi_local_redirect: spec.cgi_local_redirect.then_some(true),
        sendfile_roots: spec.sendfile_roots.clone(),
        sendfile_header: spec.sendfile_header.clone(),
        error_pages: spec.error_pages.clone(),
        error_override: spec.error_override.clone(),
        maintenance: spec.maintenance.clone(),
    }
}

/// The environment as the table that declares it, secrets as references.
fn env_table_from(env: &[EnvSpec]) -> BTreeMap<String, muru_config::EnvValue> {
    env.iter()
        .map(|entry| {
            let value = match (&entry.secret_store, &entry.secret_key) {
                (Some(store), Some(key)) => {
                    muru_config::EnvValue::Secret(muru_config::SecretValue {
                        secret: muru_config::SecretRef {
                            store: store.clone(),
                            key: key.clone(),
                        },
                        deliver: entry.as_file.then(|| "file".to_owned()),
                    })
                }
                _ => muru_config::EnvValue::Plain(entry.value.clone().unwrap_or_default()),
            };

            (entry.name.clone(), value)
        })
        .collect()
}

/// Section 7.6: a soft timeout is a duration, and the hard timeout, the
/// backstop for a request the soft timeout could not interrupt, must
/// exceed it by at least 5 s.
pub fn check_timeouts(timeouts: &crate::model::WorkerTimeouts) -> Result<(), String> {
    if timeouts.soft_timeout_ms == Some(0) {
        return Err("soft_timeout must be longer than nothing".to_owned());
    }

    if timeouts.loop_stall_timeout_ms == 0 {
        return Err("loop_stall_timeout must be longer than nothing".to_owned());
    }

    if timeouts.gil_stall_timeout_ms == 0 {
        return Err("gil_stall_timeout must be longer than nothing".to_owned());
    }

    if let (Some(soft), Some(hard)) = (timeouts.soft_timeout_ms, timeouts.hard_timeout_ms)
        && hard < soft + 5_000
    {
        return Err("hard_timeout must exceed soft_timeout by at least 5s".to_owned());
    }

    Ok(())
}

/// Section 9.9: a managed HTTP release's server settings as one phrase,
/// or `workers` for a release with none.
fn describe_http(http: Option<&HttpServerSpec>) -> String {
    let Some(http) = http else {
        return "workers".to_owned();
    };

    format!(
        "{} {:?} bind {} instances {} readiness {}/{}{}",
        http.profile.as_str(),
        http.command,
        http.bind.as_str(),
        http.instances,
        http.readiness_path.as_deref().unwrap_or("any"),
        format_ms(http.readiness_timeout_ms),
        http.stop_signal
            .as_ref()
            .map(|signal| format!(" stop {signal}"))
            .unwrap_or_default()
    )
}

/// What a release runs, for the report: entrypoint, processes by threads
/// and the source.
fn describe(spec: &ReleaseSpec) -> String {
    if let Some(http) = &spec.http {
        return format!("{} in {}", http.describe(), spec.source.display());
    }

    format!(
        "{} {}x{} from {}{}",
        spec.entrypoint,
        spec.processes,
        spec.threads,
        spec.source.display(),
        if spec.scaling.is_static() {
            String::new()
        } else {
            format!(", scaling {}", spec.scaling.describe())
        }
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
        "server",
        describe_http(declared.http.as_ref()),
        describe_http(recorded.http.as_ref()),
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
        "reflection",
        declared.reflection.to_string(),
        recorded.reflection.to_string(),
    );
    compare(
        "grpc_web_origins",
        declared.grpc_web_origins.join(", "),
        recorded.grpc_web_origins.join(", "),
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
    compare(
        "static",
        describe_static(&declared.static_dirs),
        describe_static(&recorded.static_dirs),
    );
    compare(
        "accel_redirect",
        declared.accel_redirect.to_string(),
        recorded.accel_redirect.to_string(),
    );
    compare(
        "cgi_local_redirect",
        declared.cgi_local_redirect.to_string(),
        recorded.cgi_local_redirect.to_string(),
    );
    compare(
        "sendfile_roots",
        declared.sendfile_roots.join(", "),
        recorded.sendfile_roots.join(", "),
    );
    compare(
        "sendfile_header",
        declared.sendfile_header.clone().unwrap_or_default(),
        recorded.sendfile_header.clone().unwrap_or_default(),
    );
    compare(
        "max_requests",
        declared
            .max_requests
            .map(|value| value.to_string())
            .unwrap_or_else(|| "off".to_owned()),
        recorded
            .max_requests
            .map(|value| value.to_string())
            .unwrap_or_else(|| "off".to_owned()),
    );
    compare(
        "max_lifetime",
        describe_ms(declared.max_lifetime_ms),
        describe_ms(recorded.max_lifetime_ms),
    );
    compare(
        "soft_timeout",
        describe_ms(declared.timeouts.soft_timeout_ms),
        describe_ms(recorded.timeouts.soft_timeout_ms),
    );
    compare(
        "hard_timeout",
        describe_ms(declared.timeouts.hard_timeout_ms),
        describe_ms(recorded.timeouts.hard_timeout_ms),
    );
    compare(
        "hard_drain_grace",
        describe_ms(Some(declared.timeouts.hard_drain_grace_ms)),
        describe_ms(Some(recorded.timeouts.hard_drain_grace_ms)),
    );
    compare(
        "shutdown_timeout",
        describe_ms(Some(declared.timeouts.shutdown_timeout_ms)),
        describe_ms(Some(recorded.timeouts.shutdown_timeout_ms)),
    );
    compare(
        "websocket_drain_grace",
        describe_ms(Some(declared.timeouts.websocket_drain_grace_ms)),
        describe_ms(Some(recorded.timeouts.websocket_drain_grace_ms)),
    );
    compare(
        "loop_stall_timeout",
        describe_ms(Some(declared.timeouts.loop_stall_timeout_ms)),
        describe_ms(Some(recorded.timeouts.loop_stall_timeout_ms)),
    );
    compare(
        "gil_stall_timeout",
        describe_ms(Some(declared.timeouts.gil_stall_timeout_ms)),
        describe_ms(Some(recorded.timeouts.gil_stall_timeout_ms)),
    );
    compare(
        "memory",
        describe_memory(&declared.memory),
        describe_memory(&recorded.memory),
    );
    compare(
        "env",
        describe_env(&declared.env),
        describe_env(&recorded.env),
    );
    compare(
        "scaling",
        declared.scaling.describe(),
        recorded.scaling.describe(),
    );
    compare(
        "error_pages",
        describe_error_pages(&declared.error_pages),
        describe_error_pages(&recorded.error_pages),
    );
    compare(
        "error_override",
        describe_error_override(declared.error_override.as_ref()),
        describe_error_override(recorded.error_override.as_ref()),
    );
    compare(
        "maintenance",
        describe_maintenance(declared.maintenance.as_ref()),
        describe_maintenance(recorded.maintenance.as_ref()),
    );

    found
}

/// Error pages as one phrase: each key and what it is.
fn describe_error_pages(pages: &BTreeMap<String, muru_config::ErrorPage>) -> String {
    if pages.is_empty() {
        return "none".to_owned();
    }

    pages
        .iter()
        .map(
            |(key, page)| match (&page.file, &page.template, &page.path) {
                (Some(file), _, _) => format!("{key} file {file}"),
                (_, Some(_), _) => format!("{key} template"),
                (_, _, Some(path)) => format!("{key} path {path}"),
                _ => key.clone(),
            },
        )
        .collect::<Vec<_>>()
        .join(", ")
}

/// An error override as one phrase.
fn describe_error_override(table: Option<&muru_config::ErrorOverride>) -> String {
    match table {
        Some(table) => format!(
            "{} when {}",
            table
                .statuses
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            table.when.as_deref().unwrap_or("all")
        ),
        None => "off".to_owned(),
    }
}

/// Maintenance exemptions as one phrase.
fn describe_maintenance(table: Option<&muru_config::Maintenance>) -> String {
    let Some(table) = table else {
        return "no exemptions".to_owned();
    };
    let mut parts = Vec::new();

    if !table.exempt_paths.is_empty() {
        parts.push(format!("paths {}", table.exempt_paths.join(", ")));
    }

    if !table.exempt_addresses.is_empty() {
        parts.push(format!("addresses {}", table.exempt_addresses.join(", ")));
    }

    if table.bypass_cookie.is_some() {
        parts.push("a bypass cookie".to_owned());
    }

    if let Some(retry) = &table.retry_after {
        parts.push(format!("retry after {retry}"));
    }

    if parts.is_empty() {
        "no exemptions".to_owned()
    } else {
        parts.join(", ")
    }
}

/// An environment as one phrase, for the report, never with a secret's
/// value.
fn describe_env(env: &[EnvSpec]) -> String {
    if env.is_empty() {
        return "none".to_owned();
    }

    env.iter()
        .map(EnvSpec::describe)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A memory policy as one phrase, for the report.
fn describe_memory(memory: &crate::model::MemoryPolicy) -> String {
    let size = |bytes: Option<u64>| {
        bytes
            .map(muru_config::format_size)
            .unwrap_or_else(|| "off".to_owned())
    };
    format!(
        "{} soft {} for {} (reclaim first {}), hard {}, every {}",
        memory.measure,
        size(memory.soft_limit),
        describe_ms(Some(memory.soft_for_ms)),
        memory.reclaim_first,
        size(memory.hard_limit),
        describe_ms(Some(memory.check_interval_ms)),
    )
}

/// A duration from a configuration table in milliseconds, none when the
/// key is absent.
fn parse_ms(text: Option<&String>, name: &str) -> Result<Option<u64>, String> {
    text.map(|text| {
        muru_config::parse_duration(text)
            .map(|duration| duration.as_millis() as u64)
            .map_err(|error| format!("{name}: {error}"))
    })
    .transpose()
}

/// A duration in milliseconds as the configuration writes it, or off.
fn describe_ms(ms: Option<u64>) -> String {
    match ms {
        Some(ms) => muru_config::format_duration(Duration::from_millis(ms)),
        None => "off".to_owned(),
    }
}

/// Groups as one phrase, for the report.
fn describe_groups(groups: &[GroupSpec]) -> String {
    if groups.is_empty() {
        return "none".to_owned();
    }

    groups
        .iter()
        .map(|group| {
            let mut on = match (group.paths.is_empty(), group.client_class.is_empty()) {
                (true, true) => "every path".to_owned(),
                (_, true) => group.paths.join(" "),
                (true, false) => format!("every path for {}", group.client_class.join(" ")),
                (false, false) => {
                    format!(
                        "{} for {}",
                        group.paths.join(" "),
                        group.client_class.join(" ")
                    )
                }
            };

            if !group.host.is_empty() {
                on.push_str(&format!(" host {}", group.host.join(" ")));
            }

            if !group.method.is_empty() {
                on.push_str(&format!(" method {}", group.method.join(" ")));
            }

            if !group.mounts.is_empty() {
                on.push_str(&format!(" mounts {}", group.mounts.join(" ")));
            }

            let own = [
                ("soft_timeout", group.timeouts.soft_timeout_ms),
                ("hard_timeout", group.timeouts.hard_timeout_ms),
                ("hard_drain_grace", group.timeouts.hard_drain_grace_ms),
                ("loop_stall_timeout", group.timeouts.loop_stall_timeout_ms),
                ("gil_stall_timeout", group.timeouts.gil_stall_timeout_ms),
            ]
            .into_iter()
            .filter_map(|(name, ms)| ms.map(|ms| format!("{name} {}", describe_ms(Some(ms)))))
            .collect::<Vec<_>>();

            format!(
                "{} {}x{} on {on}{}{}{}{}",
                group.name,
                group.processes,
                group.threads,
                if group.optional { " optional" } else { "" },
                if group.scaling.is_static() {
                    String::new()
                } else {
                    format!(" scaling {}", group.scaling.describe())
                },
                if own.is_empty() {
                    String::new()
                } else {
                    format!(" timeouts {}", own.join(", "))
                },
                group
                    .memory
                    .as_ref()
                    .map(|memory| format!(" memory {}", describe_memory(memory)))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The release's own directories as one phrase, for the report.
fn describe_static(entries: &[StaticSpec]) -> String {
    if entries.is_empty() {
        return "none".to_owned();
    }

    entries
        .iter()
        .map(|entry| format!("{} from {}", entry.path, entry.dir.display()))
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
        .map(|mount| {
            let mut flags = Vec::new();

            if mount.internal {
                flags.push("internal".to_owned());
            }

            if !mount.accept_redirects_from.is_empty() {
                flags.push(format!(
                    "redirects from {}",
                    mount.accept_redirects_from.join(", ")
                ));
            }

            if mount.accel_redirect {
                flags.push("accel_redirect".to_owned());
            }

            if mount.cgi_local_redirect {
                flags.push("cgi_local_redirect".to_owned());
            }

            if !mount.sendfile_roots.is_empty() {
                flags.push(format!("sendfile from {}", mount.sendfile_roots.join(", ")));
            }

            if let Some(threads) = mount.threads {
                flags.push(format!("threads {}", threads.describe()));
            }

            let flags = if flags.is_empty() {
                String::new()
            } else {
                format!(" ({})", flags.join(", "))
            };
            format!(
                "{} {} at {}{flags}",
                mount.name, mount.entrypoint, mount.path
            )
        })
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
            &Tables::default(),
            &crate::provision::Provisioning::off(),
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

        let plan = plan(
            &store,
            &Document::parse(&text).unwrap(),
            true,
            &Defined::default(),
            &crate::provision::Provisioning::off(),
        );
        assert!(plan.errors.is_empty(), "{:?}", plan.errors);
        assert!(plan.changes.is_empty(), "{:?}", plan.changes);
        assert_eq!(plan.unchanged, 1);
    }

    #[test]
    fn a_release_records_the_layer_each_setting_came_from() {
        let source = source_dir();
        let store = store_with_shop(&source);
        let document = export(&store, None, false).unwrap();
        let lines = release_layers(&store, &document).unwrap();
        let line = |key: &str| {
            lines
                .iter()
                .find(|line| line.starts_with(&format!("shop@r1: {key} = ")))
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(
            line("entrypoint"),
            "shop@r1: entrypoint = shop.wsgi:application (deploy flags)"
        );
        assert_eq!(line("threads"), "shop@r1: threads = 4 (default)");
        assert_eq!(
            line("timeouts.soft_timeout"),
            "shop@r1: timeouts.soft_timeout = off (default)"
        );

        // A deploy takes what it does not give from the release before;
        // a declared release names its table.

        let previous = store.release("shop", "r1").unwrap().spec;
        let request = pb::DeployRequest {
            app: "shop".to_owned(),
            source: source.display().to_string(),
            threads: Some(2),
            ..Default::default()
        };
        let off = crate::provision::Provisioning::off();
        let next = resolve_spec(&request, Some(&previous), &Tables::default(), &off).unwrap();
        assert_eq!(next.layers.0["entrypoint"], "the previous release");
        assert_eq!(next.layers.0["threads"], "deploy flags");
        assert_eq!(next.layers.0["sync_threads"], "the previous release");

        let declared = Tables {
            declared: true,
            ..Tables::default()
        };
        let spec = resolve_spec(&request, Some(&previous), &declared, &off).unwrap();
        assert_eq!(spec.layers.0["threads"], "the release table");
        assert_eq!(spec, next, "layers are not settings");
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

        let plan = plan(
            &store,
            &Document::parse(&text).unwrap(),
            false,
            &Defined::default(),
            &crate::provision::Provisioning::off(),
        );
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
    fn applied_classes_keep_their_place_and_limits_may_be_named() {
        let store = Store::in_memory().unwrap();
        let file = Defined {
            rate_limits: vec!["crawlers".to_owned()],
            client_classes: vec!["partner".to_owned()],
            ..Defined::default()
        };
        let class =
            |name: &str| format!("[[client_class]]\nname = '{name}'\nuser_agent = ['{name}']\n");
        let placed = |plan: &Plan| -> Vec<(String, i64)> {
            plan.changes
                .iter()
                .filter_map(|change| match &change.action {
                    Action::PutClientClass {
                        class, position, ..
                    } => Some((class.name.clone(), *position)),
                    _ => None,
                })
                .collect()
        };
        let record = |plan: &Plan| {
            for change in &plan.changes {
                if let Action::PutClientClass {
                    class, position, ..
                } = &change.action
                {
                    store.put_client_class(class, *position).unwrap();
                }
            }
        };

        // A host may draw from a limit the same document defines.

        let text = format!(
            "{}{}[rate_limit.slow]\nrate = '1/s'\n\
             [host.shop]\nnames = ['shop.example.com']\nrate_limits = ['slow']\n",
            class("late"),
            class("early")
        );
        let first = plan(
            &store,
            &Document::parse(&text).unwrap(),
            false,
            &file,
            &crate::provision::Provisioning::off(),
        );
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert_eq!(
            placed(&first),
            [("late".to_owned(), 0), ("early".to_owned(), 1)]
        );
        record(&first);

        let again = format!("{}{}", class("late"), class("early"));
        assert!(
            plan(
                &store,
                &Document::parse(&again).unwrap(),
                false,
                &file,
                &crate::provision::Provisioning::off(),
            )
            .changes
            .is_empty()
        );

        // A new class goes after those recorded; with prune the
        // document's order is the order.

        let added = plan(
            &store,
            &Document::parse(&class("extra")).unwrap(),
            false,
            &file,
            &crate::provision::Provisioning::off(),
        );
        assert_eq!(placed(&added), [("extra".to_owned(), 2)]);

        let reordered = format!("{}{}", class("early"), class("late"));
        let pruned = plan(
            &store,
            &Document::parse(&reordered).unwrap(),
            true,
            &file,
            &crate::provision::Provisioning::off(),
        );
        assert_eq!(
            placed(&pruned),
            [("early".to_owned(), 0), ("late".to_owned(), 1)]
        );

        let refused = format!(
            "{}[[client_class]]\nname = 'listed'\nuser_agent_list = 'bots.txt'\n\
             [rate_limit.crawlers]\nrate = '1/s'\n",
            class("partner")
        );
        let errors = plan(
            &store,
            &Document::parse(&refused).unwrap(),
            false,
            &file,
            &crate::provision::Provisioning::off(),
        )
        .errors
        .join("\n");

        for expected in [
            "client_class partner: the daemon configuration file defines it",
            "client_class listed: the list bots.txt must be an absolute path",
            "rate_limit crawlers: the daemon configuration file defines it",
        ] {
            assert!(errors.contains(expected), "{expected}: {errors}");
        }
    }

    #[test]
    fn routes_for_client_classes_share_a_path() {
        let source = source_dir();
        let store = store_with_shop(&source);
        let defined = Defined {
            client_classes: vec!["partner".to_owned(), "ai_crawler".to_owned()],
            ..Defined::default()
        };
        let routes = |extra: &str| {
            format!(
                "[host.shop]\nnames = ['shop.example.com']\n\
                 [[route]]\nhost = 'shop'\napp = 'shop'\n\
                 [[route]]\nhost = 'shop'\napp = 'shop'\nmatch = {{ client_class = ['partner'] }}\n\
                 traffic = {{ classes = {{ ai_crawler = 'r1' }} }}\n{extra}"
            )
        };

        let plan = plan(
            &store,
            &Document::parse(&routes("")).unwrap(),
            false,
            &defined,
            &crate::provision::Provisioning::off(),
        );
        assert!(plan.errors.is_empty(), "{:?}", plan.errors);
        let subjects: Vec<String> = plan.changes.iter().map(Change::subject).collect();
        assert!(
            subjects.contains(&"route shop /".to_owned())
                && subjects.contains(&"route shop / client_class=partner".to_owned()),
            "{subjects:?}"
        );

        let text = routes(
            "[[route]]\nhost = 'shop'\napp = 'shop'\nmatch = { client_class = ['partner', 'bots'] }\n\
             traffic = { classes = { robots = 'r9' } }\n",
        );
        let refused = super::plan(
            &store,
            &Document::parse(&text).unwrap(),
            false,
            &defined,
            &crate::provision::Provisioning::off(),
        );
        let errors = refused.errors.join("\n");

        for expected in [
            "no client class named bots",
            "no client class named robots",
            "traffic names shop@r9, which does not exist",
            "another route with the path is for client class partner",
        ] {
            assert!(errors.contains(expected), "{expected}: {errors}");
        }
    }

    #[test]
    fn prune_removes_what_is_not_declared() {
        let source = source_dir();
        let store = store_with_shop(&source);
        let plan = plan(
            &store,
            &Document::default(),
            true,
            &Defined::default(),
            &crate::provision::Provisioning::off(),
        );
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

        let plan = plan(
            &store,
            &Document::parse(text).unwrap(),
            false,
            &Defined::default(),
            &crate::provision::Provisioning::off(),
        );
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

    #[test]
    fn the_hard_timeout_must_back_the_soft_one_up() {
        let timeouts = |soft: Option<u64>, hard: Option<u64>| crate::model::WorkerTimeouts {
            soft_timeout_ms: soft,
            hard_timeout_ms: hard,
            ..Default::default()
        };
        assert_eq!(check_timeouts(&timeouts(None, None)), Ok(()));
        assert_eq!(check_timeouts(&timeouts(Some(30_000), None)), Ok(()));
        assert_eq!(
            check_timeouts(&timeouts(Some(30_000), Some(35_000))),
            Ok(())
        );
        assert_eq!(
            check_timeouts(&timeouts(Some(30_000), Some(34_999))),
            Err("hard_timeout must exceed soft_timeout by at least 5s".to_owned())
        );
        assert_eq!(
            check_timeouts(&timeouts(Some(0), None)),
            Err("soft_timeout must be longer than nothing".to_owned())
        );
    }

    #[test]
    fn scaling_tables_resolve_with_defaults_and_are_checked() {
        let (spec, processes) = resolve_scaling(None, 2, "scaling").unwrap();
        assert!(spec.is_static());
        assert_eq!((spec.max_workers, processes), (2, 2));

        let table = muru_config::Scaling {
            mode: Some(ScalingMode::Spare),
            max_workers: Some(12),
            min_workers: Some(3),
            ..muru_config::Scaling::default()
        };
        let (spec, processes) = resolve_scaling(Some(&table), 1, "scaling").unwrap();
        assert_eq!(processes, 3, "raised to the floor");
        assert_eq!((spec.min_spare, spec.max_spare), (1, 3));
        assert_eq!(spec.describe(), "spare 3-12 workers, 1-3 idle");
        assert_eq!(
            scaling_table(&spec).unwrap().scale_in_window.as_deref(),
            Some("600s")
        );
        assert!(scaling_table(&ScalingSpec::default()).is_none());

        let refused = |table: muru_config::Scaling, processes: u32| {
            resolve_scaling(Some(&table), processes, "scaling").unwrap_err()
        };
        assert!(
            refused(
                muru_config::Scaling {
                    mode: Some(ScalingMode::Target),
                    ..muru_config::Scaling::default()
                },
                1
            )
            .contains("needs max_workers")
        );
        assert!(
            refused(
                muru_config::Scaling {
                    max_workers: Some(4),
                    ..muru_config::Scaling::default()
                },
                1
            )
            .contains("takes no other keys")
        );
        assert!(
            refused(
                muru_config::Scaling {
                    mode: Some(ScalingMode::Spare),
                    max_workers: Some(4),
                    ..muru_config::Scaling::default()
                },
                5
            )
            .contains("over max_workers")
        );
        assert!(
            refused(
                muru_config::Scaling {
                    mode: Some(ScalingMode::Target),
                    max_workers: Some(4),
                    queue_wait_p95: Some("soon".to_owned()),
                    ..muru_config::Scaling::default()
                },
                1
            )
            .contains("queue_wait_p95")
        );

        // Section 7.4: a group may scale to zero, and only one that does
        // may stop its host when idle.

        let to_zero = muru_config::Scaling {
            mode: Some(ScalingMode::Spare),
            min_workers: Some(0),
            max_workers: Some(4),
            host_idle_timeout: Some("10m".to_owned()),
            ..muru_config::Scaling::default()
        };
        let (spec, processes) = resolve_scaling(Some(&to_zero), 1, "scaling").unwrap();
        assert_eq!((spec.min_workers, processes), (0, 1));
        assert_eq!(spec.host_idle_timeout_ms, Some(600_000));
        assert_eq!(
            scaling_table(&spec).unwrap().host_idle_timeout.as_deref(),
            Some("600s")
        );
        assert!(
            refused(
                muru_config::Scaling {
                    min_workers: Some(1),
                    ..to_zero
                },
                1
            )
            .contains("host_idle_timeout needs min_workers = 0")
        );
    }
}
