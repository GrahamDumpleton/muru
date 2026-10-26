//! The daemon's dashboard and JSON gateway (design specification section
//! 10.7), served on an internal address from the daemon's own data: each
//! request asks the reconciler for a fresh picture of the node, the
//! charts draw the history the reconciler records every second, and the
//! events stream carries each event as it happens.

use std::sync::Arc;

use muru_dashboard::{
    Action, Actions, EventRow, EventsSource, History, NodeMetricsSource, NodeSource, Reading,
    Sessions,
};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::daemon::Command;
use crate::events::Events;

/// Serve the dashboard on the listener until accepting fails, taking
/// actions from the sessions given, when it is.
pub async fn serve(
    listener: TcpListener,
    commands: mpsc::Sender<Command>,
    history: Arc<History>,
    events: Events,
    sessions: Option<Arc<Sessions>>,
) -> anyhow::Result<()> {
    let actions = sessions.map(|sessions| Actions {
        sessions,
        sink: action_sink(commands.clone(), events.clone()),
    });
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
    muru_dashboard::serve_node(listener, source, metrics, history, live, actions).await
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
/// A reading of the request counters for the history, from the samples
/// the edge reports: requests and 5xx responses completed, the total
/// time histogram, requests in flight.
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
            "muru_requests_in_flight" => reading.in_flight += sample.value as u64,
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
