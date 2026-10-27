//! Overlays (design specification section 9.10): named partial
//! configuration laid over the routes and applications while active. The
//! base is never changed, so an overlay that ends simply stops applying,
//! and what changed in the base meanwhile shows through. This module
//! holds what follows from the declarations, the state and the clock;
//! the daemon carries it out.

use std::collections::{BTreeMap, BTreeSet};

use muru_config::{ClientMatch, Methods, Overlay, Route, RouteOverlay, Traffic};

use crate::model::OverlayState;
use crate::scaling::Capacity;

/// The name of the overlay `muructl scale --min` makes for a worker group
/// of an application: `APP:scale`, or `APP:scale:GROUP` for a group other
/// than the default. No declared overlay or schedule has a `:`.
pub fn temporary_name(app: &str, group: &str) -> String {
    if group == muru_edge::routing::DEFAULT_GROUP {
        format!("{app}:scale")
    } else {
        format!("{app}:scale:{group}")
    }
}

/// Whether an overlay stands for an application's `[[app.NAME.schedule]]`
/// entry (section 7.5), named `APP.NAME`, whose activation and end are
/// the events schedule.activated and schedule.expired.
pub fn is_schedule(name: &str) -> bool {
    name.contains('.') && !is_temporary(name)
}

/// Whether an overlay is one `muructl scale --min` made, which apply
/// leaves alone and which is forgotten once it ends.
pub fn is_temporary(name: &str) -> bool {
    name.contains(':')
}

/// The window of an overlay's schedule that `now` falls in, else the
/// next one, as Unix milliseconds: from its start, until its end when it
/// has one. None without a schedule, with no window to come, or for one
/// that does not parse (apply refuses those).
pub fn window(overlay: &Overlay, now: u64) -> Option<(u64, Option<u64>)> {
    let schedule = overlay.trigger.as_ref()?.schedule.as_ref()?;
    let (start, end) = muru_config::schedule_window(schedule, now as i64)?;
    Some((start.max(0) as u64, end.map(|end| end.max(0) as u64)))
}

/// Whether `now` falls in a window of the overlay's schedule.
pub fn in_window(overlay: &Overlay, now: u64) -> bool {
    window(overlay, now).is_some_and(|(start, end)| now >= start && end.is_none_or(|end| now < end))
}

/// Whether `now` falls in the lead before a window of the overlay's
/// schedule, when it has a warm-up.
pub fn warming(overlay: &Overlay, now: u64) -> bool {
    let Some(lead) = overlay
        .warm_up
        .as_ref()
        .and_then(|warm_up| muru_config::parse_duration(&warm_up.lead).ok())
    else {
        return false;
    };

    window(overlay, now)
        .is_some_and(|(start, _)| now < start && now + lead.as_millis() as u64 >= start)
}

/// What starts an overlay, as the reports show it.
pub fn describe_trigger(overlay: &Overlay) -> String {
    let external = overlay
        .trigger
        .as_ref()
        .is_some_and(|trigger| trigger.external == Some(true));

    let Some(schedule) = overlay
        .trigger
        .as_ref()
        .and_then(|trigger| trigger.schedule.as_ref())
    else {
        return if external {
            "by hand or webhook".to_owned()
        } else {
            "by hand".to_owned()
        };
    };

    let mut text = muru_config::describe_schedule(schedule);

    if external {
        text.push_str(", webhook");
    }

    if overlay.advisory == Some(true) {
        text.push_str(", advisory");
    }

    if overlay.latch == Some(true) {
        text.push_str(", latched");
    }

    text
}

/// A wait as the events say it, to the minute once it is that long:
/// 45s, 59m, 2h, 1h30m.
pub fn describe_wait(millis: u64) -> String {
    let minutes = millis / 60_000;

    match (minutes / 60, minutes % 60) {
        (0, 0) => format!("{}s", millis / 1000),
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h{minutes}m"),
    }
}

/// What should happen to an overlay now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing changes.
    Stay,
    /// Its schedule's window has begun: activate it if its preconditions
    /// hold, or for an advisory overlay say it would.
    Begin {
        /// When the window ends.
        until: Option<u64>,
    },
    /// Its window ended as the next began, the recurring windows
    /// overlapping: it stays active, until the new one's end.
    Extend {
        /// When the new window ends.
        until: Option<u64>,
    },
    /// It is active and should not be: end it.
    End {
        /// Why, for the event.
        reason: String,
    },
    /// A hold has run out: forget it, so the schedule decides again.
    Release,
}

/// What should happen to an overlay at `now`, given its state.
pub fn step(overlay: &Overlay, state: Option<&OverlayState>, now: u64) -> Step {
    let window = window(overlay, now);
    let in_window =
        window.is_some_and(|(start, end)| now >= start && end.is_none_or(|end| now < end));
    let latched = overlay.latch == Some(true);

    match state {
        Some(state) if state.active => {
            let by_schedule = state.source == "schedule";

            if state.until_unix_ms.is_some_and(|until| now >= until) {
                if by_schedule && in_window {
                    return Step::Extend {
                        until: window.and_then(|(_, end)| end),
                    };
                }

                return Step::End {
                    reason: if by_schedule {
                        "its schedule's window is over".to_owned()
                    } else {
                        "the time it was activated for is over".to_owned()
                    },
                };
            }

            if by_schedule && !in_window && !latched {
                return Step::End {
                    reason: "its schedule no longer covers now".to_owned(),
                };
            }

            Step::Stay
        }
        Some(state) => {
            // Held: ended by hand within its window, skipped, advised, or
            // promoted.

            match state.until_unix_ms {
                Some(until) if now >= until => Step::Release,
                _ => Step::Stay,
            }
        }
        None if in_window => Step::Begin {
            until: window.and_then(|(_, end)| end),
        },
        None => Step::Stay,
    }
}

/// The active overlays in the order they apply: lowest priority first,
/// so a higher one's setting stands, then by name.
pub fn ordered<'a>(
    overlays: &'a [(String, Overlay)],
    states: &[OverlayState],
) -> Vec<&'a (String, Overlay)> {
    let active: BTreeSet<&str> = states
        .iter()
        .filter(|state| state.active)
        .map(|state| state.name.as_str())
        .collect();
    let mut chosen: Vec<&(String, Overlay)> = overlays
        .iter()
        .filter(|(name, _)| active.contains(name.as_str()))
        .collect();
    chosen.sort_by(|a, b| {
        a.1.priority
            .unwrap_or(0)
            .cmp(&b.1.priority.unwrap_or(0))
            .then_with(|| a.0.cmp(&b.0))
    });
    chosen
}

/// Lay the overlays' route changes over `routes`, in order. Each route
/// `client_routes` names for a class is the route for that class alone at
/// the route's host and path, or else a copy of the route, changed, added
/// ahead of it.
pub fn apply_routes(routes: &mut Vec<Route>, overlays: &[&(String, Overlay)]) {
    for (_, overlay) in overlays {
        for (name, change) in &overlay.routes {
            let Some(mut index) = routes
                .iter()
                .position(|route| route.name.as_ref() == Some(name))
            else {
                continue;
            };

            change_route(&mut routes[index], change);

            for (class, class_change) in change.client_routes.iter().flatten() {
                let named = &routes[index];
                let found = routes.iter().position(|route| {
                    route.host == named.host
                        && route.path == named.path
                        && route.classes() == std::slice::from_ref(class)
                });

                match found {
                    Some(at) => change_route(&mut routes[at], class_change),
                    None => {
                        let mut added = routes[index].clone();
                        added.name = None;
                        added.matches = Some(ClientMatch {
                            client_class: vec![class.clone()],
                        });
                        change_route(&mut added, class_change);
                        routes.insert(index, added);
                        index += 1;
                    }
                }
            }
        }

        // Section 4.15: an application's mode and methods go on each of
        // its routes.

        for (app, change) in &overlay.apps {
            let as_route = RouteOverlay {
                mode: change.mode.clone(),
                deny: change.deny.clone(),
                answer: change.answer.clone(),
                ..RouteOverlay::default()
            };

            for route in routes.iter_mut().filter(|route| &route.app == app) {
                change_route(route, &as_route);
            }
        }
    }
}

/// Lay one overlay's change over a route.
fn change_route(route: &mut Route, change: &RouteOverlay) {
    if let Some(mode) = &change.mode {
        route.mode = Some(mode.clone());
    }

    if let Some(deny) = &change.deny {
        route.methods = Some(Methods {
            deny: deny.clone(),
            answer: change.answer.clone(),
        });
    }

    if let Some(limits) = &change.rate_limits {
        route.rate_limits = limits.clone();
    }

    // Section 4.12: each page replaces the route's for its key.

    for (key, page) in change.error_pages.iter().flatten() {
        route.error_pages.insert(key.clone(), page.clone());
    }

    if let Some(target) = &change.target {
        route.app = target.app.clone().unwrap_or_default();
        route.static_dir = target.static_dir.clone();
        route.upstream = target.upstream.clone().unwrap_or_default();

        if target.app.is_none() {
            route.traffic = None;
        }
    }

    if let Some(traffic) = &change.traffic {
        let mut policy = route.traffic.clone().unwrap_or_default();
        policy.split = match (&traffic.pin, &traffic.split) {
            (Some(release), _) => BTreeMap::from([(release.clone(), 100)]),
            (None, Some(split)) => split.clone(),
            (None, None) => policy.split,
        };
        route.traffic = Some(policy);
    }
}

/// The applications in maintenance mode with the overlays applied: the
/// base set, with each overlay's `maintenance` in order.
pub fn apply_maintenance(
    base: &BTreeSet<String>,
    overlays: &[&(String, Overlay)],
) -> BTreeSet<String> {
    let mut apps = base.clone();

    for (_, overlay) in overlays {
        for (app, change) in &overlay.apps {
            match change.maintenance {
                Some(true) => {
                    apps.insert(app.clone());
                }
                Some(false) => {
                    apps.remove(app);
                }
                None => {}
            }
        }
    }

    apps
}

/// Section 7.5: the capacity the overlays set, by application and worker
/// group: the highest floor and ceiling, and a hold on scale-in if any
/// sets one.
pub fn capacities<'a>(
    overlays: impl IntoIterator<Item = &'a (String, Overlay)>,
) -> BTreeMap<(String, String), Capacity> {
    let mut found: BTreeMap<(String, String), Capacity> = BTreeMap::new();

    for (name, overlay) in overlays {
        for (app, change) in &overlay.apps {
            if !change.sets_capacity() {
                continue;
            }

            let group = change
                .group
                .clone()
                .unwrap_or_else(|| muru_edge::routing::DEFAULT_GROUP.to_owned());
            let capacity = found.entry((app.clone(), group)).or_default();
            capacity.floor = capacity.floor.max(change.min_workers.unwrap_or(0));
            capacity.ceiling = capacity.ceiling.max(change.max_workers.unwrap_or(0));
            capacity.hold |= change.scale_in.as_deref() == Some("hold");
            capacity.sources.push(name.clone());
        }
    }

    found
}

/// Section 9.6: the applications whose deploys and rollouts the overlays
/// freeze, with the overlays that do.
pub fn freezes(overlays: &[&(String, Overlay)]) -> BTreeMap<String, Vec<String>> {
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (name, overlay) in overlays {
        for (app, change) in &overlay.apps {
            if change.freeze_deploys == Some(true) {
                found.entry(app.clone()).or_default().push(name.clone());
            }
        }
    }

    found
}

/// The applications whose traffic an active overlay pins or splits on
/// one of their routes, whose rollouts wait meanwhile.
pub fn pinned_apps(routes: &[Route], overlays: &[&(String, Overlay)]) -> BTreeSet<String> {
    let mut apps = BTreeSet::new();

    for (_, overlay) in overlays {
        for route in routes {
            let Some(change) = route
                .name
                .as_ref()
                .and_then(|name| overlay.routes.get(name))
            else {
                continue;
            };

            // A route for a class serves the application its target
            // names, or the route's.

            let for_classes = change
                .client_routes
                .iter()
                .flatten()
                .map(|(_, class_change)| {
                    class_change
                        .target
                        .as_ref()
                        .map_or(Some(&route.app), |target| target.app.as_ref())
                        .filter(|_| class_change.traffic.is_some())
                });

            let route_app = change.traffic.is_some().then_some(&route.app);

            for app in std::iter::once(route_app).chain(for_classes).flatten() {
                if !app.is_empty() {
                    apps.insert(app.clone());
                }
            }
        }
    }

    apps
}

/// Each field two overlays both set, as an error, unless they have
/// different priorities to settle it.
pub fn conflicts(overlays: &BTreeMap<String, Overlay>) -> Vec<String> {
    let mut setters: BTreeMap<String, Vec<(&str, Option<i32>)>> = BTreeMap::new();

    for (name, overlay) in overlays {
        for (route, change) in &overlay.routes {
            let fields = [
                ("mode", change.mode.is_some()),
                ("methods", change.deny.is_some()),
                ("traffic", change.traffic.is_some()),
                ("target", change.target.is_some()),
                ("rate_limits", change.rate_limits.is_some()),
            ];

            for (field, set) in fields {
                if set {
                    setters
                        .entry(format!("routes.{route}.{field}"))
                        .or_default()
                        .push((name, overlay.priority));
                }
            }

            for key in change.error_pages.iter().flat_map(BTreeMap::keys) {
                setters
                    .entry(format!("routes.{route}.error_pages.{key}"))
                    .or_default()
                    .push((name, overlay.priority));
            }

            for class in change.client_routes.iter().flat_map(BTreeMap::keys) {
                setters
                    .entry(format!("routes.{route}.client_routes.{class}"))
                    .or_default()
                    .push((name, overlay.priority));
            }
        }

        for (app, change) in &overlay.apps {
            let fields = [
                ("maintenance", change.maintenance.is_some()),
                ("mode", change.mode.is_some()),
                ("methods", change.deny.is_some()),
            ];

            for (field, set) in fields {
                if set {
                    setters
                        .entry(format!("apps.{app}.{field}"))
                        .or_default()
                        .push((name, overlay.priority));
                }
            }
        }
    }

    let mut errors = Vec::new();

    for (field, names) in setters {
        if names.len() < 2 {
            continue;
        }

        let priorities: BTreeSet<Option<i32>> =
            names.iter().map(|(_, priority)| *priority).collect();

        if priorities.len() < names.len() || priorities.contains(&None) {
            let named: Vec<&str> = names.iter().map(|(name, _)| *name).collect();
            errors.push(format!(
                "overlays {} all set {field}: give each a different priority",
                named.join(", ")
            ));
        }
    }

    errors
}

/// One field an overlay changes, against the routes and maintenance set
/// as they stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldChange {
    /// What it is on: `route NAME` or `app NAME`.
    pub subject: String,
    /// The field, or a phrase for what it sets.
    pub field: String,
    /// The value before, for a field that has one.
    pub before: Option<String>,
    /// The value after.
    pub after: String,
}

impl FieldChange {
    fn line(&self) -> String {
        let show = |value: &str| {
            if value.is_empty() {
                "(none)".to_owned()
            } else {
                value.to_owned()
            }
        };

        match &self.before {
            Some(before) => format!(
                "{}: {} {} -> {}",
                self.subject,
                self.field,
                show(before),
                show(&self.after)
            ),
            None if self.after.is_empty() => format!("{}: {}", self.subject, self.field),
            None => format!("{}: {}: {}", self.subject, self.field, self.after),
        }
    }
}

/// What activating `overlay` would change, one line per field, against
/// the routes as they stand.
pub fn preview(overlay: &Overlay, routes: &[Route], maintenance: &BTreeSet<String>) -> Vec<String> {
    changes(overlay, routes, maintenance)
        .iter()
        .map(FieldChange::line)
        .collect()
}

/// Section 9.10: what the active overlays set that is in force, one line
/// per field, for `config show --effective`. They apply in order, so a
/// later overlay's setting of a field replaces an earlier one's.
pub fn attribute(
    routes: &[Route],
    maintenance: &BTreeSet<String>,
    active: &[&(String, Overlay)],
) -> Vec<String> {
    let mut current = routes.to_vec();
    let mut maintained = maintenance.clone();
    let mut found: BTreeMap<(String, String), (FieldChange, String)> = BTreeMap::new();

    for pair in active {
        let (name, overlay) = (&pair.0, &pair.1);

        for change in changes(overlay, &current, &maintained) {
            // Capacity and freezes add up rather than replace, so each
            // overlay's stays listed.

            let key = match change.before {
                Some(_) => (change.subject.clone(), change.field.clone()),
                None => (change.subject.clone(), format!("{} {name}", change.field)),
            };

            found.insert(key, (change, name.clone()));
        }

        apply_routes(&mut current, &[*pair]);
        maintained = apply_maintenance(&maintained, &[*pair]);
    }

    found
        .into_values()
        .map(|(change, name)| {
            let what = if is_schedule(&name) {
                "schedule"
            } else {
                "overlay"
            };

            let line = match &change.before {
                Some(_) if change.after.is_empty() => {
                    format!("{}: {} = (none)", change.subject, change.field)
                }
                Some(_) => format!("{}: {} = {}", change.subject, change.field, change.after),
                None => change.line(),
            };

            format!("{line} ({what} {name})")
        })
        .collect()
}

/// The fields `overlay` changes against the routes and maintenance set.
fn changes(
    overlay: &Overlay,
    routes: &[Route],
    maintenance: &BTreeSet<String>,
) -> Vec<FieldChange> {
    let mut found = Vec::new();
    let pair = (String::new(), overlay.clone());
    let mut changed = routes.to_vec();
    apply_routes(&mut changed, &[&pair]);

    for (name, change) in &overlay.routes {
        let subject = format!("route {name}");

        let Some(before) = routes
            .iter()
            .find(|route| route.name.as_deref() == Some(name))
        else {
            found.push(FieldChange {
                subject,
                field: "no route has this name".to_owned(),
                before: None,
                after: String::new(),
            });
            continue;
        };

        let Some(after) = changed
            .iter()
            .find(|route| route.name.as_deref() == Some(name))
        else {
            continue;
        };

        let fields = [
            (
                "mode",
                before.mode.clone().unwrap_or_default(),
                after.mode.clone().unwrap_or_default(),
            ),
            (
                "methods.deny",
                before
                    .methods
                    .as_ref()
                    .map(|methods| methods.deny.join(", "))
                    .unwrap_or_default(),
                after
                    .methods
                    .as_ref()
                    .map(|methods| methods.deny.join(", "))
                    .unwrap_or_default(),
            ),
            ("target", describe_target(before), describe_target(after)),
            (
                "traffic.split",
                describe_split(before.traffic.as_ref()),
                describe_split(after.traffic.as_ref()),
            ),
            (
                "rate_limits",
                before.rate_limits.join(", "),
                after.rate_limits.join(", "),
            ),
        ];

        let pages = change
            .error_pages
            .iter()
            .flat_map(BTreeMap::keys)
            .map(|key| {
                (
                    format!("error_pages.{key}"),
                    describe_page(before.error_pages.get(key)),
                    describe_page(after.error_pages.get(key)),
                )
            });

        // A route for a class, at the route's host and path.

        let for_class = |all: &[Route], class: &String| {
            describe_route(all.iter().find(|route| {
                route.host == before.host
                    && route.path == before.path
                    && route.classes() == std::slice::from_ref(class)
            }))
        };

        let classes = change
            .client_routes
            .iter()
            .flat_map(BTreeMap::keys)
            .map(|class| {
                (
                    format!("client_routes.{class}"),
                    for_class(routes, class),
                    for_class(&changed, class),
                )
            });

        for (field, was, will) in fields
            .into_iter()
            .map(|(field, was, will)| (field.to_owned(), was, will))
            .chain(pages)
            .chain(classes)
        {
            if was != will {
                found.push(FieldChange {
                    subject: subject.clone(),
                    field,
                    before: Some(was),
                    after: will,
                });
            }
        }
    }

    for (app, change) in &overlay.apps {
        let subject = format!("app {app}");

        // Section 4.15: the mode and methods on its routes, each value
        // they have now listed once.

        let on_routes = |all: &[Route], value: fn(&Route) -> String| -> String {
            let values: BTreeSet<String> = all
                .iter()
                .filter(|route| &route.app == app)
                .map(value)
                .collect();
            values.into_iter().collect::<Vec<_>>().join(" | ")
        };
        let mode = |route: &Route| route.mode.clone().unwrap_or_default();
        let deny = |route: &Route| {
            route
                .methods
                .as_ref()
                .map(|methods| methods.deny.join(", "))
                .unwrap_or_default()
        };

        for (field, set, value) in [
            ("mode", change.mode.is_some(), mode as fn(&Route) -> String),
            ("methods.deny", change.deny.is_some(), deny),
        ] {
            let (was, will) = (on_routes(routes, value), on_routes(&changed, value));

            if set && was != will {
                found.push(FieldChange {
                    subject: subject.clone(),
                    field: field.to_owned(),
                    before: Some(was),
                    after: will,
                });
            }
        }

        if let Some(on) = change.maintenance {
            let was = maintenance.contains(app);
            let state = |on: bool| if on { "on" } else { "off" }.to_owned();

            if was != on {
                found.push(FieldChange {
                    subject: subject.clone(),
                    field: "maintenance".to_owned(),
                    before: Some(state(was)),
                    after: state(on),
                });
            }
        }

        if change.sets_capacity() {
            found.push(FieldChange {
                subject: subject.clone(),
                field: format!(
                    "group {}",
                    change
                        .group
                        .as_deref()
                        .unwrap_or(muru_edge::routing::DEFAULT_GROUP)
                ),
                before: None,
                after: describe_capacity(change),
            });
        }

        if change.freeze_deploys == Some(true) {
            found.push(FieldChange {
                subject,
                field: "deploys and rollouts frozen".to_owned(),
                before: None,
                after: String::new(),
            });
        }
    }

    found
}

/// The capacity an overlay sets on an application, as a phrase.
pub fn describe_capacity(change: &muru_config::AppOverlay) -> String {
    let mut parts = Vec::new();

    if let Some(floor) = change.min_workers {
        parts.push(format!("at least {floor} workers"));
    }

    if let Some(ceiling) = change.max_workers {
        parts.push(format!("at most {ceiling}"));
    }

    if change.scale_in.as_deref() == Some("hold") {
        parts.push("no scale-in".to_owned());
    }

    parts.join(", ")
}

/// An error page as one phrase: what makes it.
fn describe_page(page: Option<&muru_config::ErrorPage>) -> String {
    let Some(page) = page else {
        return String::new();
    };

    if let Some(file) = &page.file {
        format!("file {file}")
    } else if let Some(path) = &page.path {
        format!("path {path}")
    } else {
        "a template".to_owned()
    }
}

/// A route for a client class as one phrase: what serves it, and the
/// policies it has.
fn describe_route(route: Option<&Route>) -> String {
    let Some(route) = route else {
        return String::new();
    };

    let mut parts = vec![describe_target(route)];

    if let Some(mode) = &route.mode {
        parts.push(format!("mode {mode}"));
    }

    if let Some(methods) = &route.methods {
        parts.push(format!("deny {}", methods.deny.join(", ")));
    }

    if !route.rate_limits.is_empty() {
        parts.push(format!("rate limits {}", route.rate_limits.join(", ")));
    }

    let split = describe_split(route.traffic.as_ref());

    if !split.is_empty() {
        parts.push(format!("traffic {split}"));
    }

    parts.join("; ")
}

fn describe_target(route: &Route) -> String {
    if let Some(directory) = &route.static_dir {
        format!("static {directory}")
    } else if !route.upstream.is_empty() {
        format!("upstream {}", route.upstream)
    } else {
        format!("app {}", route.app)
    }
}

fn describe_split(traffic: Option<&Traffic>) -> String {
    traffic
        .map(|traffic| {
            traffic
                .split
                .iter()
                .map(|(release, percent)| format!("{release} {percent}%"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(text: &str) -> Overlay {
        toml::from_str(text).unwrap()
    }

    fn route(name: &str, app: &str) -> Route {
        Route {
            name: Some(name.to_owned()),
            host: "site".to_owned(),
            path: "/".to_owned(),
            app: app.to_owned(),
            ..Route::default()
        }
    }

    fn state(active: bool, source: &str, until: Option<u64>) -> OverlayState {
        OverlayState {
            name: "o".to_owned(),
            active,
            source: source.to_owned(),
            since_unix_ms: 0,
            until_unix_ms: until,
        }
    }

    #[test]
    fn a_schedule_begins_and_ends_its_window() {
        let backup = overlay(
            "apps.shop = { maintenance = true }\ntrigger = { schedule = { start = '2026-01-01T02:00:00Z', end = '2026-01-01T03:00:00Z' } }",
        );
        let (start, end) = window(&backup, 0).unwrap();
        let end = end.unwrap();
        assert_eq!(end - start, 3_600_000);

        assert_eq!(step(&backup, None, start - 1), Step::Stay);
        assert_eq!(step(&backup, None, start), Step::Begin { until: Some(end) });
        let active = state(true, "schedule", Some(end));
        assert_eq!(step(&backup, Some(&active), start + 10), Step::Stay);
        assert!(matches!(
            step(&backup, Some(&active), end),
            Step::End { .. }
        ));

        // Ended by hand inside the window, it stays ended until the
        // window is over, and then the schedule decides again.

        let held = state(false, "ended", Some(end));
        assert_eq!(step(&backup, Some(&held), start + 10), Step::Stay);
        assert_eq!(step(&backup, Some(&held), end), Step::Release);
    }

    const HOUR: u64 = 3_600_000;

    /// 2026-01-01T00:00:00Z.
    const NEW_YEAR: u64 = 1_767_225_600_000;

    #[test]
    fn a_recurring_schedule_opens_a_window_at_each_time() {
        let nightly = overlay(
            "apps.shop = { maintenance = true }\ntrigger = { schedule = { cron = '0 2 * * *', duration = '1h', timezone = 'UTC' } }",
        );
        let two = NEW_YEAR + 2 * HOUR;
        assert_eq!(window(&nightly, NEW_YEAR), Some((two, Some(two + HOUR))));
        assert_eq!(step(&nightly, None, two - 1), Step::Stay);
        assert_eq!(
            step(&nightly, None, two),
            Step::Begin {
                until: Some(two + HOUR)
            }
        );

        // After the window, the next is the following night's.

        assert!(!in_window(&nightly, two + HOUR));
        assert_eq!(
            window(&nightly, two + HOUR),
            Some((two + 24 * HOUR, Some(two + 25 * HOUR)))
        );
        let active = state(true, "schedule", Some(two + HOUR));
        assert!(matches!(
            step(&nightly, Some(&active), two + HOUR),
            Step::End { .. }
        ));
    }

    #[test]
    fn overlapping_recurring_windows_extend_an_active_overlay() {
        let busy = overlay(
            "apps.shop = { maintenance = true }\ntrigger = { schedule = { cron = '0 * * * *', duration = '90m', timezone = 'UTC' } }",
        );
        let active = state(true, "schedule", Some(NEW_YEAR + 90 * 60_000));
        assert_eq!(
            step(&busy, Some(&active), NEW_YEAR + 90 * 60_000),
            Step::Extend {
                until: Some(NEW_YEAR + HOUR + 90 * 60_000)
            }
        );
    }

    #[test]
    fn a_latched_overlay_outlasts_its_window() {
        let latched = overlay(
            "apps.shop = { maintenance = true }\ntrigger = { schedule = { start = '2026-01-01T02:00:00Z', end = '2026-01-01T03:00:00Z' } }\nlatch = true",
        );
        let active = state(true, "schedule", None);
        assert_eq!(
            step(&latched, Some(&active), NEW_YEAR + 5 * HOUR),
            Step::Stay
        );
    }

    #[test]
    fn warm_up_covers_the_lead_before_each_window() {
        let cutover = overlay(
            "routes.root.traffic = { pin = 'r2' }\ntrigger = { schedule = { cron = '0 9 * * *', duration = '1h', timezone = 'UTC' } }\nwarm_up = { lead = '30m', start_release = 'r2' }",
        );
        let nine = NEW_YEAR + 9 * HOUR;
        assert!(!warming(&cutover, nine - HOUR));
        assert!(warming(&cutover, nine - HOUR / 2));
        assert!(warming(&cutover, nine - 1));
        assert!(!warming(&cutover, nine));
        assert!(warming(&cutover, nine + 24 * HOUR - 60_000));
        assert_eq!(describe_trigger(&cutover), "cron '0 9 * * *' for 1h UTC");
        assert_eq!(describe_wait(45_000), "45s");
        assert_eq!(describe_wait(HOUR - 1), "59m");
        assert_eq!(describe_wait(HOUR * 3 / 2), "1h30m");
    }

    #[test]
    fn capacity_takes_the_highest_floor_and_ceiling_and_freezes_add_up() {
        let declared = [
            (
                "shop.weekday".to_owned(),
                overlay("apps.shop = { min_workers = 4, max_workers = 16 }"),
            ),
            (
                "shop.black-friday".to_owned(),
                overlay(
                    "apps.shop = { min_workers = 24, max_workers = 12, scale_in = 'hold', freeze_deploys = true }",
                ),
            ),
            (
                "reports".to_owned(),
                overlay("apps.shop = { group = 'reports', min_workers = 2 }"),
            ),
        ];
        let all: Vec<&(String, Overlay)> = declared.iter().collect();
        let found = capacities(all.iter().copied());
        let web = &found[&("shop".to_owned(), "default".to_owned())];
        assert_eq!((web.floor, web.ceiling, web.hold), (24, 16, true));
        assert_eq!(web.sources, ["shop.weekday", "shop.black-friday"]);
        assert_eq!(found[&("shop".to_owned(), "reports".to_owned())].floor, 2);
        assert_eq!(
            freezes(&all),
            BTreeMap::from([("shop".to_owned(), vec!["shop.black-friday".to_owned()])])
        );

        // Capacity never conflicts.

        assert!(conflicts(&declared.into_iter().collect()).is_empty());
        assert_eq!(temporary_name("shop", "default"), "shop:scale");
        assert!(is_temporary(&temporary_name("shop", "reports")));
    }

    #[test]
    fn a_manual_activation_lasts_until_its_time_or_for_ever() {
        let manual = overlay("apps.shop = { maintenance = true }");
        assert_eq!(step(&manual, None, 10), Step::Stay);
        assert_eq!(
            step(&manual, Some(&state(true, "manual", None)), u64::MAX),
            Step::Stay
        );
        assert!(matches!(
            step(&manual, Some(&state(true, "manual", Some(100))), 100),
            Step::End { .. }
        ));
    }

    #[test]
    fn overlays_apply_in_priority_order_over_the_base() {
        let declared = vec![
            (
                "read-only".to_owned(),
                overlay("routes.root = { mode = 'read-only', deny = ['POST'] }\npriority = 2"),
            ),
            (
                "limited".to_owned(),
                overlay(
                    "routes.root = { mode = 'limited', traffic = { pin = 'r9' } }\npriority = 1",
                ),
            ),
            (
                "idle".to_owned(),
                overlay("apps.shop = { maintenance = true }"),
            ),
        ];
        let states = vec![
            OverlayState {
                name: "read-only".to_owned(),
                ..state(true, "manual", None)
            },
            OverlayState {
                name: "limited".to_owned(),
                ..state(true, "manual", None)
            },
        ];
        let active = ordered(&declared, &states);
        assert_eq!(
            active
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["limited", "read-only"]
        );

        let mut routes = vec![route("root", "shop"), route("other", "shop")];
        apply_routes(&mut routes, &active);
        assert_eq!(routes[0].mode.as_deref(), Some("read-only"));
        assert_eq!(routes[0].methods.as_ref().unwrap().deny, ["POST"]);
        assert_eq!(
            routes[0].traffic.as_ref().unwrap().split,
            BTreeMap::from([("r9".to_owned(), 100)])
        );
        assert_eq!(routes[1], route("other", "shop"));
        assert_eq!(
            pinned_apps(&routes, &active),
            BTreeSet::from(["shop".to_owned()])
        );

        // The inactive overlay's maintenance does not apply.

        assert!(apply_maintenance(&BTreeSet::new(), &active).is_empty());
    }

    #[test]
    fn a_target_replaces_what_serves_the_route() {
        let snapshot = [(
            "outage".to_owned(),
            overlay("routes.root = { target = { static = '/srv/snapshot' } }"),
        )];
        let mut routes = vec![route("root", "shop")];
        routes[0].traffic = Some(Traffic::default());
        apply_routes(&mut routes, &snapshot.iter().collect::<Vec<_>>());
        assert_eq!(routes[0].app, "");
        assert_eq!(routes[0].static_dir.as_deref(), Some("/srv/snapshot"));
        assert!(routes[0].traffic.is_none());
    }

    #[test]
    fn error_pages_replace_the_routes_page_for_their_key() {
        let outage = [(
            "outage".to_owned(),
            overlay(
                "routes.root.error_pages = { '503' = { file = '/srv/down.html' }, '502' = { template = 'gone' } }",
            ),
        )];
        let mut routes = vec![route("root", "shop")];
        routes[0].error_pages = BTreeMap::from([
            (
                "503".to_owned(),
                muru_config::ErrorPage {
                    path: Some("/errors/503".to_owned()),
                    ..muru_config::ErrorPage::default()
                },
            ),
            (
                "404".to_owned(),
                muru_config::ErrorPage {
                    path: Some("/errors/404".to_owned()),
                    ..muru_config::ErrorPage::default()
                },
            ),
        ]);

        assert_eq!(
            preview(&outage[0].1, &routes, &BTreeSet::new()),
            [
                "route root: error_pages.502 (none) -> a template",
                "route root: error_pages.503 path /errors/503 -> file /srv/down.html",
            ]
        );

        apply_routes(&mut routes, &outage.iter().collect::<Vec<_>>());
        let pages = &routes[0].error_pages;
        assert_eq!(pages["503"].file.as_deref(), Some("/srv/down.html"));
        assert_eq!(pages["502"].template.as_deref(), Some("gone"));
        assert_eq!(pages["404"].path.as_deref(), Some("/errors/404"));

        // Two overlays choosing a page for the same key need priorities;
        // pages for different keys do not conflict.

        let declared = BTreeMap::from([
            ("outage".to_owned(), outage[0].1.clone()),
            (
                "notice".to_owned(),
                overlay(
                    "routes.root.error_pages = { '503' = { template = 'later' }, '500' = { template = 'oops' } }",
                ),
            ),
        ]);
        assert_eq!(
            conflicts(&declared),
            [
                "overlays notice, outage all set routes.root.error_pages.503: give each a different priority"
            ]
        );
    }

    #[test]
    fn client_routes_add_or_change_the_route_for_a_class() {
        let surge = (
            "surge".to_owned(),
            overlay(
                "routes.root = { mode = 'read-only', client_routes = { ai_crawler = { target = { static = '/srv/snapshot' } }, partner = { rate_limits = ['slow'] } } }",
            ),
        );
        let partner = Route {
            host: "site".to_owned(),
            path: "/".to_owned(),
            matches: Some(ClientMatch {
                client_class: vec!["partner".to_owned()],
            }),
            static_dir: Some("/srv/partner".to_owned()),
            ..Route::default()
        };
        let mut routes = vec![route("root", "shop"), partner];

        assert_eq!(
            preview(&surge.1, &routes, &BTreeSet::new()),
            [
                "route root: mode (none) -> read-only",
                "route root: client_routes.ai_crawler (none) -> static /srv/snapshot; mode read-only",
                "route root: client_routes.partner static /srv/partner -> static /srv/partner; rate limits slow",
            ]
        );

        // The crawlers' route is a copy of the route, ahead of it; the
        // partners' route is the one already there.

        let later = (
            "later".to_owned(),
            overlay("priority = 1\nroutes.root.client_routes.ai_crawler = { mode = 'degraded' }"),
        );
        apply_routes(&mut routes, &[&surge, &later]);
        assert_eq!(routes.len(), 3);
        assert_eq!(routes[0].name, None);
        assert_eq!(routes[0].classes(), ["ai_crawler"]);
        assert_eq!(routes[0].static_dir.as_deref(), Some("/srv/snapshot"));
        assert_eq!(routes[0].mode.as_deref(), Some("degraded"));
        assert_eq!(routes[1].name.as_deref(), Some("root"));
        assert_eq!(routes[1].mode.as_deref(), Some("read-only"));
        assert_eq!(routes[2].rate_limits, ["slow"]);

        let declared = BTreeMap::from([
            surge.clone(),
            (
                "pinned".to_owned(),
                overlay("routes.root.client_routes.ai_crawler = { traffic = { pin = 'r1' } }"),
            ),
        ]);
        assert_eq!(
            conflicts(&declared),
            [
                "overlays pinned, surge all set routes.root.client_routes.ai_crawler: give each a different priority"
            ]
        );

        // A class's pin holds the application's rollout as the route's
        // would.

        let pinned = ("pinned".to_owned(), declared["pinned"].clone());
        assert_eq!(
            pinned_apps(&[route("root", "shop")], &[&pinned]),
            BTreeSet::from(["shop".to_owned()])
        );
        assert!(pinned_apps(&[route("root", "shop")], &[&surge]).is_empty());
    }

    #[test]
    fn overlays_setting_the_same_field_need_priorities() {
        let mut declared = BTreeMap::from([
            (
                "a".to_owned(),
                overlay("routes.root = { mode = 'read-only' }"),
            ),
            (
                "b".to_owned(),
                overlay("routes.root = { mode = 'limited' }"),
            ),
            (
                "c".to_owned(),
                overlay("apps.shop = { maintenance = true }"),
            ),
        ]);
        let errors = conflicts(&declared);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("overlays a, b all set routes.root.mode"),
            "{errors:?}"
        );

        declared.get_mut("a").unwrap().priority = Some(1);
        declared.get_mut("b").unwrap().priority = Some(2);
        assert!(conflicts(&declared).is_empty());
    }

    #[test]
    fn an_applications_mode_goes_on_each_of_its_routes() {
        let mut api = route("api", "shop");
        api.path = "/api".to_owned();
        let base = vec![route("root", "shop"), api, route("blog", "blog")];
        let declared = [(
            "backup".to_owned(),
            overlay("apps.shop = { mode = 'read-only', deny = ['POST'] }"),
        )];
        let active: Vec<&(String, Overlay)> = declared.iter().collect();
        let mut routes = base.clone();
        apply_routes(&mut routes, &active);
        assert_eq!(routes[0].mode.as_deref(), Some("read-only"));
        assert_eq!(routes[1].methods.as_ref().unwrap().deny, ["POST"]);
        assert_eq!(routes[2].mode, None, "another application's route");

        assert_eq!(
            preview(&declared[0].1, &base, &BTreeSet::new()),
            [
                "app shop: mode (none) -> read-only",
                "app shop: methods.deny (none) -> POST",
            ]
        );
        assert_eq!(
            attribute(&base, &BTreeSet::new(), &active),
            [
                "app shop: methods.deny = POST (overlay backup)",
                "app shop: mode = read-only (overlay backup)",
            ]
        );

        let twice = BTreeMap::from([
            declared[0].clone(),
            (
                "again".to_owned(),
                overlay("apps.shop = { mode = 'limited' }"),
            ),
        ]);
        assert!(conflicts(&twice)[0].contains("all set apps.shop.mode"));
    }

    #[test]
    fn the_effective_values_are_attributed_to_the_overlay_in_force() {
        let declared = [
            (
                "limited".to_owned(),
                overlay("routes.root = { mode = 'limited', deny = ['POST'] }\npriority = 1"),
            ),
            (
                "read-only".to_owned(),
                overlay("routes.root = { mode = 'read-only' }\npriority = 2"),
            ),
            (
                "shop.surge".to_owned(),
                overlay("apps.shop = { min_workers = 4, freeze_deploys = true }"),
            ),
        ];
        let active: Vec<&(String, Overlay)> = declared.iter().collect();
        let lines = attribute(&[route("root", "shop")], &BTreeSet::new(), &active);
        assert_eq!(
            lines,
            [
                "app shop: deploys and rollouts frozen (schedule shop.surge)",
                "app shop: group default: at least 4 workers (schedule shop.surge)",
                "route root: methods.deny = POST (overlay limited)",
                "route root: mode = read-only (overlay read-only)",
            ]
        );
    }

    #[test]
    fn a_preview_lists_what_would_change() {
        let backup = overlay(
            "routes.root = { mode = 'read-only', deny = ['POST', 'PUT'] }\nroutes.gone = { mode = 'x' }\napps.shop = { maintenance = true }",
        );
        let lines = preview(&backup, &[route("root", "shop")], &BTreeSet::new());
        assert_eq!(
            lines,
            [
                "route gone: no route has this name",
                "route root: mode (none) -> read-only",
                "route root: methods.deny (none) -> POST, PUT",
                "app shop: maintenance off -> on",
            ]
        );
    }
}
