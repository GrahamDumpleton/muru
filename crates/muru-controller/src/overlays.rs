//! Overlays (design specification section 9.10): named partial
//! configuration laid over the routes and applications while active. The
//! base is never changed, so an overlay that ends simply stops applying,
//! and what changed in the base meanwhile shows through. This module
//! holds what follows from the declarations, the state and the clock;
//! the daemon carries it out.

use std::collections::{BTreeMap, BTreeSet};

use muru_config::{Methods, Overlay, Route, Traffic};

use crate::model::OverlayState;

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
    let Some(schedule) = overlay
        .trigger
        .as_ref()
        .and_then(|trigger| trigger.schedule.as_ref())
    else {
        return "by hand".to_owned();
    };

    let mut text = muru_config::describe_schedule(schedule);

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
