//! The daemon's dashboard and JSON gateway (design specification section
//! 10.7), served on an internal address from the daemon's own data: each
//! request asks the reconciler for a fresh picture of the node, the
//! charts draw the history the reconciler records every second, and the
//! events stream carries each event as it happens.

use std::sync::Arc;

use muru_dashboard::{
    Action, Actions, DebugAnswer, DebugAsk, DebugSource, EndpointRow, EventRow, EventsSource,
    History, NodeMetricsSource, NodeSource, Reading, Sessions, SocketRow, WebSocketsView,
    WorkerAnswer,
};
use muru_proto::edge::v1 as pb;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::daemon::Command;
use crate::events::Events;

/// Serve the dashboard on the listener until accepting fails, for the
/// sessions given, which also take the actions when they may.
pub async fn serve(
    listener: TcpListener,
    commands: mpsc::Sender<Command>,
    history: Arc<History>,
    events: Events,
    sessions: Arc<Sessions>,
) -> anyhow::Result<()> {
    let actions = Actions {
        sessions,
        sink: action_sink(commands.clone(), events.clone()),
    };
    let debug = debug_source(commands.clone());
    let data_commands = commands.clone();
    let source: NodeSource = Arc::new(move || {
        let commands = data_commands.clone();
        Box::pin(async move {
            let (done_tx, done_rx) = oneshot::channel();
            commands
                .send(Command::Dashboard { done: done_tx })
                .await
                .ok()?;
            done_rx.await.ok()
        })
    });
    let metrics: NodeMetricsSource = Arc::new(move || {
        let commands = commands.clone();
        Box::pin(async move {
            let (done_tx, done_rx) = oneshot::channel();
            commands
                .send(Command::Metrics { done: done_tx })
                .await
                .ok()?;
            done_rx.await.ok()
        })
    });

    // Events arrive as the store records them and go out as rows.

    let (rows, _) = broadcast::channel::<EventRow>(256);
    let mut incoming = events.subscribe();
    let forward = rows.clone();
    tokio::spawn(async move {
        loop {
            match incoming.recv().await {
                Ok(event) => {
                    let _ = forward.send(EventRow {
                        sequence: event.sequence,
                        time: crate::daemon::clock_hms(event.time_unix_ms),
                        level: event.level.to_owned(),
                        kind: event.kind,
                        subject: event.subject,
                        message: event.message,
                    });
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    let live: EventsSource = Arc::new(move || rows.subscribe());
    muru_dashboard::serve_node(listener, source, metrics, history, live, actions, debug).await
}

/// Section 11.7: ask a release's workers what the inspection pages show,
/// as the admin API's Debug does, with the arguments `muructl debug`
/// sends for the operation and the same wait for the host.
fn debug_source(commands: mpsc::Sender<Command>) -> DebugSource {
    Arc::new(move |ask: DebugAsk| {
        let commands = commands.clone();
        Box::pin(async move {
            let args = if ask.op == "memory" {
                serde_json::json!({
                    "trace": false,
                    "snapshot": "",
                    "duration_s": 0.0,
                    "top": 20,
                    "timeout_s": 30.0,
                })
            } else {
                serde_json::json!({})
            };
            let wait = args
                .get("timeout_s")
                .and_then(serde_json::Value::as_f64)
                .map_or(std::time::Duration::from_secs(20), |seconds| {
                    std::time::Duration::from_secs_f64(seconds) + std::time::Duration::from_secs(15)
                });
            let (done_tx, done_rx) = oneshot::channel();
            commands
                .send(Command::Debug {
                    app: ask.app,
                    id: ask.release.clone(),
                    op: ask.op,
                    worker: ask.worker,
                    args,
                    done: done_tx,
                })
                .await
                .map_err(|_| "the daemon is stopping".to_owned())?;
            let replies = tokio::time::timeout(wait, done_rx)
                .await
                .map_err(|_| format!("the host did not answer in {} seconds", wait.as_secs()))?
                .map_err(|_| "the daemon is stopping".to_owned())??;
            Ok(DebugAnswer {
                release: ask.release,
                workers: replies
                    .into_iter()
                    .map(|reply| WorkerAnswer {
                        index: reply.index,
                        pid: reply.pid,
                        ok: reply.ok,
                        error: (!reply.error.is_empty()).then_some(reply.error),
                        result: reply
                            .ok
                            .then(|| serde_json::from_str(&reply.result_json).ok())
                            .flatten(),
                    })
                    .collect(),
            })
        })
    })
}

/// Section 10.7: carry out a dashboard action as the admin API would,
/// recording who asked as a `dashboard.action` event.
fn action_sink(commands: mpsc::Sender<Command>, events: Events) -> muru_dashboard::ActionSink {
    Arc::new(move |action, session| {
        let commands = commands.clone();
        let events = events.clone();
        Box::pin(async move {
            let Action::Maintenance { app, on } = action;
            let (done_tx, done_rx) = oneshot::channel();
            commands
                .send(Command::SetMaintenance {
                    app: app.clone(),
                    on,
                    done: done_tx,
                })
                .await
                .map_err(|_| "the daemon is stopping".to_owned())?;
            done_rx
                .await
                .map_err(|_| "the daemon is stopping".to_owned())??;
            let what = format!(
                "maintenance {} for {app} from the dashboard, session {session}",
                if on { "on" } else { "off" }
            );
            events.emit("dashboard.action", &app, "notice", &what);
            Ok(what)
        })
    })
}
/// The WebSockets an edge reported, as the dashboards show them.
pub fn websockets_view(report: pb::WebSockets) -> WebSocketsView {
    WebSocketsView {
        open: report.open as usize,
        endpoints: report
            .endpoints
            .into_iter()
            .map(|endpoint| EndpointRow {
                app: endpoint.app,
                pattern: endpoint.pattern,
                open: endpoint.open as usize,
            })
            .collect(),
        sockets: report
            .sockets
            .into_iter()
            .map(|socket| SocketRow {
                release: if socket.app.is_empty() {
                    "-".to_owned()
                } else {
                    format!("{}@{}", socket.app, socket.release)
                },
                path: socket.path,
                client: socket.client,
                opened: crate::daemon::clock_hms(socket.opened_unix_ms),
                age: muru_dashboard::uptime(std::time::Duration::from_millis(
                    crate::model::now_unix_ms().saturating_sub(socket.opened_unix_ms),
                )),
                received: socket.received,
                sent: socket.sent,
                worker: if socket.worker < 0 {
                    "-".to_owned()
                } else {
                    socket.worker.to_string()
                },
                request_id: socket.request_id,
            })
            .collect(),
    }
}

/// A reading of the request counters for the history, from the samples
/// the edge reports: requests and 5xx responses completed, the total
/// time histogram, requests in flight and WebSockets open.
pub fn reading(samples: &[muru_edge::metrics::Sample], workers: u64) -> Reading {
    let mut reading = Reading {
        workers,
        ..Reading::default()
    };
    let mut buckets: std::collections::BTreeMap<u64, (f64, f64)> =
        std::collections::BTreeMap::new();
    let label = |sample: &muru_edge::metrics::Sample, name: &str| -> Option<String> {
        sample
            .labels
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };

    for sample in samples {
        match sample.name.as_str() {
            "muru_requests_total" => {
                reading.requests_total += sample.value;

                if label(sample, "status").as_deref() == Some("5xx") {
                    reading.errors_total += sample.value;
                }
            }
            "muru_requests_in_flight" => reading.in_flight += sample.value.max(0.0) as u64,
            "muru_websocket_connections" => reading.websockets += sample.value.max(0.0) as u64,
            "muru_request_duration_seconds_bucket"
                if label(sample, "phase").as_deref() == Some("total") =>
            {
                let bound = match label(sample, "le").as_deref() {
                    Some("+Inf") => f64::INFINITY,
                    Some(text) => text.parse().unwrap_or(f64::INFINITY),
                    None => continue,
                };

                let key = bound.to_bits();
                let entry = buckets.entry(key).or_insert((bound, 0.0));
                entry.1 += sample.value;
            }
            _ => {}
        }
    }

    let mut ordered: Vec<(f64, f64)> = buckets.into_values().collect();
    ordered.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    reading.buckets = ordered;
    reading
}
