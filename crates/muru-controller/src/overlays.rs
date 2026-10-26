//! Overlays (design specification section 9.10): named partial
//! configuration laid over the routes and applications while active. The
//! base is never changed, so an overlay that ends simply stops applying,
//! and what changed in the base meanwhile shows through. This module
//! holds what follows from the declarations, the state and the clock;
//! the daemon carries it out.

use std::collections::{BTreeMap, BTreeSet};

use muru_config::{Methods, Overlay, Route, Traffic};

use crate::model::OverlayState;

/// The window an overlay's schedule gives, as Unix milliseconds: from its
/// start, until its end when it has one. None without a schedule, or for
/// one that does not parse (apply refuses those).
pub fn window(overlay: &Overlay) -> Option<(u64, Option<u64>)> {
    let schedule = overlay.trigger.as_ref()?.schedule.as_ref()?;
    let zone = schedule.timezone.as_deref();
    let start = muru_config::parse_moment(&schedule.start, zone).ok()?;

    let end = match &schedule.end {
        Some(end) => Some(muru_config::parse_moment(end, zone).ok()?),
        None => None,
    };

    Some((start.max(0) as u64, end.map(|end| end.max(0) as u64)))
}

/// What should happen to an overlay now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing changes.
    Stay,
    /// Its schedule's window has begun: activate it, until the window's
    /// end, if its preconditions hold.
    Begin {
        /// When the window ends.
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
    let window = window(overlay);
    let in_window =
        window.is_some_and(|(start, end)| now >= start && end.is_none_or(|end| now < end));

    match state {
        Some(state) if state.active => {
            if state.until_unix_ms.is_some_and(|until| now >= until) {
                return Step::End {
                    reason: if state.source == "manual" {
                        "the time it was activated for is over".to_owned()
                    } else {
                        "its schedule's window is over".to_owned()
                    },
                };
            }

            if state.source == "schedule" && !in_window {
                return Step::End {
                    reason: "its schedule no longer covers now".to_owned(),
                };
            }

            Step::Stay
        }
        Some(state) => {
            // Held: ended by hand within its window, skipped, or promoted.

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

/// Lay the overlays' route changes over `routes`, in order.
pub fn apply_routes(routes: &mut [Route], overlays: &[&(String, Overlay)]) {
    for (_, overlay) in overlays {
        for route in routes.iter_mut() {
            let Some(change) = route
                .name
                .as_ref()
                .and_then(|name| overlay.routes.get(name))
            else {
                continue;
            };

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

            if let Some(target) = &change.target {
                route.app = target.app.clone().unwrap_or_default();
                route.static_dir = target.static_dir.clone();
                route.upstream = target.upstream.clone().unwrap_or_default();

                if target.app.is_none() {
                    route.traffic = None;
                }
            }

            if let Some(traffic) = &change.traffic {
                let mut policy = route.traffic.clone().unwrap_or_else(Traffic::default);
                policy.split = match (&traffic.pin, &traffic.split) {
                    (Some(release), _) => BTreeMap::from([(release.clone(), 100)]),
                    (None, Some(split)) => split.clone(),
                    (None, None) => policy.split,
                };
                route.traffic = Some(policy);
            }
        }
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

/// The applications whose traffic an active overlay pins or splits on
/// one of their routes, whose rollouts wait meanwhile.
pub fn pinned_apps(routes: &[Route], overlays: &[&(String, Overlay)]) -> BTreeSet<String> {
    let mut apps = BTreeSet::new();

    for (_, overlay) in overlays {
        for route in routes {
            if route
                .name
                .as_ref()
                .and_then(|name| overlay.routes.get(name))
                .is_some_and(|change| change.traffic.is_some())
                && !route.app.is_empty()
            {
                apps.insert(route.app.clone());
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
        }

        for (app, change) in &overlay.apps {
            if change.maintenance.is_some() {
                setters
                    .entry(format!("apps.{app}.maintenance"))
                    .or_default()
                    .push((name, overlay.priority));
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

/// What activating `overlay` would change, one line per field, against
/// the routes as they stand.
pub fn preview(overlay: &Overlay, routes: &[Route], maintenance: &BTreeSet<String>) -> Vec<String> {
    let mut lines = Vec::new();

    for name in overlay.routes.keys() {
        let Some(before) = routes
            .iter()
            .find(|route| route.name.as_deref() == Some(name))
        else {
            lines.push(format!("route {name}: no route has this name"));
            continue;
        };

        let pair = (String::new(), overlay.clone());
        let mut after = [before.clone()];
        apply_routes(&mut after, &[&pair]);
        let after = &after[0];
        let show = |value: String| {
            if value.is_empty() {
                "(none)".to_owned()
            } else {
                value
            }
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

        for (field, was, will) in fields {
            if was != will {
                lines.push(format!(
                    "route {name}: {field} {} -> {}",
                    show(was),
                    show(will)
                ));
            }
        }
    }

    for (app, change) in &overlay.apps {
        if let Some(on) = change.maintenance {
            let was = maintenance.contains(app);

            if was != on {
                lines.push(format!(
                    "app {app}: maintenance {} -> {}",
                    if was { "on" } else { "off" },
                    if on { "on" } else { "off" }
                ));
            }
        }
    }

    lines
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
        let (start, end) = window(&backup).unwrap();
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
