//! The requests in flight, as `muructl requests` shows them (section
//! 11.7): counted by worker group, since the edge queues per group and a
//! count per release would hide which group is starved, and listed, the
//! oldest first.

use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;

use muru_proto::edge::v1 as pb;

use crate::access::{AccessLog, Side};
use crate::routing::Routing;

type Groups = BTreeMap<(String, String, String), pb::RequestGroup>;

/// The row for a group, made when first seen.
fn row<'a>(
    groups: &'a mut Groups,
    app: &str,
    release: &str,
    group: &str,
) -> &'a mut pb::RequestGroup {
    groups
        .entry((app.to_owned(), release.to_owned(), group.to_owned()))
        .or_insert_with(|| pb::RequestGroup {
            app: app.to_owned(),
            release: release.to_owned(),
            group: group.to_owned(),
            ..pb::RequestGroup::default()
        })
}

/// The requests in flight on an edge, of one application or all: a row
/// per worker group of the releases the routing serves, with a group
/// that has nothing in flight shown at zero, and the first `limit`
/// requests, the oldest first.
pub fn report(log: &AccessLog, routing: &Routing, app: &str, limit: usize) -> pb::Requests {
    let live: Vec<_> = log
        .requests()
        .into_iter()
        .filter(|request| app.is_empty() || request.app == app)
        .collect();
    let mut groups = Groups::new();

    // The sides a group has, and its threads, come from its workers'
    // HELLOs; a managed server's release has instances, not slots.

    for pools in routing.app_pools() {
        let (name, release) = pools.labels();

        if (!app.is_empty() && name != app) || pools.server().is_some() {
            continue;
        }

        for (group, pool) in pools.pools() {
            let workers = pool.workers();
            let entry = row(&mut groups, name, release, &group);
            entry.threads = workers.iter().any(|worker| worker.threads > 0);
            entry.threads_of = clamp(workers.iter().map(|worker| worker.threads).sum());
            entry.event_loop = workers.iter().any(|worker| worker.loop_slots > 0);
        }
    }

    for request in &live {
        let entry = row(&mut groups, &request.app, &request.release, &request.group);
        let age = millis(request.age);

        if !request.running {
            entry.queued += 1;
            entry.queued_oldest_ms = entry.queued_oldest_ms.max(age);
            continue;
        }

        match request.side {
            Side::Threads => {
                entry.threads = true;
                entry.threads_busy += 1;
                entry.threads_oldest_ms = entry.threads_oldest_ms.max(age);
            }
            Side::Loop => {
                entry.event_loop = true;
                entry.event_loop_busy += 1;
                entry.event_loop_oldest_ms = entry.event_loop_oldest_ms.max(age);
            }
            Side::None => {}
        }
    }

    let requests = live
        .iter()
        .take(limit)
        .map(|request| pb::InFlightRequest {
            request_id: request.request_id.clone(),
            client: request.client.to_string(),
            app: request.app.clone(),
            release: request.release.clone(),
            group: request.group.clone(),
            method: request.method.clone(),
            target: request.target.clone(),
            state: if request.running { "running" } else { "queued" }.to_owned(),
            kind: request.kind.clone(),
            worker: request
                .worker
                .map_or(-1, |worker| i32::try_from(worker).unwrap_or(i32::MAX)),
            started_unix_ms: request.started.duration_since(UNIX_EPOCH).map_or(0, millis),
            age_ms: millis(request.age),
        })
        .collect();

    pb::Requests {
        in_flight: clamp(live.len()),
        groups: groups.into_values().collect(),
        requests,
    }
}

/// The report as the admin API answers it, which the daemon and a
/// development run both serve.
pub fn admin_list(report: pb::Requests) -> muru_proto::admin::v1::RequestList {
    use muru_proto::admin::v1 as admin;

    admin::RequestList {
        in_flight: report.in_flight,
        groups: report
            .groups
            .into_iter()
            .map(|group| admin::RequestGroup {
                app: group.app,
                release: group.release,
                group: group.group,
                queued: group.queued,
                queued_oldest_ms: group.queued_oldest_ms,
                threads: group.threads,
                threads_busy: group.threads_busy,
                threads_of: group.threads_of,
                threads_oldest_ms: group.threads_oldest_ms,
                event_loop: group.event_loop,
                event_loop_busy: group.event_loop_busy,
                event_loop_oldest_ms: group.event_loop_oldest_ms,
            })
            .collect(),
        requests: report
            .requests
            .into_iter()
            .map(|request| admin::InFlightRequest {
                request_id: request.request_id,
                client: request.client,
                app: request.app,
                release: request.release,
                group: request.group,
                method: request.method,
                target: request.target,
                state: request.state,
                kind: request.kind,
                worker: request.worker,
                started_unix_ms: request.started_unix_ms,
                age_ms: request.age_ms,
            })
            .collect(),
    }
}

fn clamp(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
