//! The data behind the development dashboard (design specification section
//! 10.7): figures from the edge's access log and worker pools, and the
//! lines the host and workers wrote.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use muru_dashboard::{Data, RequestRow, Source, Summary, Worker};
use muru_edge::Pools;
use muru_edge::access::{AccessLog, RING_CAPACITY, Record, civil, format_bytes, format_duration};

use crate::logs::Logs;

/// How far back the summary looks.
const WINDOW: Duration = Duration::from_secs(60);
/// Rows of recent requests shown.
const REQUEST_ROWS: usize = 50;
/// Log lines shown.
const LOG_LINES: usize = 200;

/// What the dashboard reads from.
pub struct Inputs {
    /// The application name.
    pub app: String,
    /// Where the application is served.
    pub listen: SocketAddr,
    /// Where it is served over TLS, when it is.
    pub https: Option<SocketAddr>,
    /// When the run started.
    pub started: Instant,
    /// The current release number.
    pub release: Arc<AtomicU32>,
    /// The worker pools.
    pub pools: Arc<Pools>,
    /// The access log.
    pub access_log: Arc<AccessLog>,
    /// Recent host and worker output.
    pub logs: Logs,
}

/// Bind the dashboard and serve it in the background. Returns its address.
pub async fn start(address: &str, inputs: Inputs) -> anyhow::Result<SocketAddr> {
    let address: SocketAddr = address
        .parse()
        .map_err(|_| anyhow::anyhow!("{address:?} is not an address such as 127.0.0.1:8081"))?;
    anyhow::ensure!(
        address.ip().is_loopback(),
        "the dashboard is only served on loopback addresses"
    );
    let listener = muru_dashboard::bind(address).await?;
    let bound = listener.local_addr()?;
    let source: Source = Arc::new(move || collect(&inputs));
    tokio::spawn(async move {
        if let Err(error) = muru_dashboard::serve(listener, source).await {
            tracing::warn!(%error, "the dashboard stopped");
        }
    });
    Ok(bound)
}

fn collect(inputs: &Inputs) -> Data {
    let now = SystemTime::now();
    let records = inputs.access_log.recent(RING_CAPACITY);
    let recent: Vec<&Record> = records
        .iter()
        .filter(|record| {
            now.duration_since(record.started)
                .map_or(true, |age| age <= WINDOW)
        })
        .collect();
    // The ring holds a bounded number of records, so under load it reaches
    // back less than a minute: the rate is taken over the span it covers,
    // not over a minute it only partly describes.
    let covered = if records.len() >= RING_CAPACITY {
        records
            .iter()
            .filter_map(|record| now.duration_since(record.started).ok())
            .max()
            .map_or(WINDOW, |oldest| oldest.min(WINDOW))
    } else {
        WINDOW
    };
    let covered = covered.max(Duration::from_secs(1));
    let mut totals: Vec<Duration> = recent.iter().map(|record| record.total).collect();
    totals.sort();
    let percentile = |fraction: f64| -> String {
        if totals.is_empty() {
            return "-".to_owned();
        }
        let index = ((totals.len() - 1) as f64 * fraction).round() as usize;
        format_duration(totals[index])
    };
    let workers: Vec<Worker> = inputs
        .pools
        .current()
        .workers()
        .into_iter()
        .map(|worker| Worker {
            index: worker.index,
            link: worker.path.display().to_string(),
            slots: worker.slots,
            in_flight: worker.in_flight,
        })
        .collect();
    let summary = Summary {
        rate: format!("{:.1}", recent.len() as f64 / covered.as_secs_f64()),
        p50: percentile(0.5),
        p95: percentile(0.95),
        errors: recent.iter().filter(|record| record.status >= 500).count(),
        in_flight: workers.iter().map(|worker| worker.in_flight).sum(),
    };
    let requests = records
        .iter()
        .rev()
        .take(REQUEST_ROWS)
        .map(|record| {
            let (_, hours, minutes, seconds, _) = civil(record.started);
            RequestRow {
                time: format!("{hours:02}:{minutes:02}:{seconds:02}"),
                status: record.status,
                status_class: record.status / 100,
                reason: record.reason.clone(),
                method: record.method.clone(),
                target: record.target.clone(),
                bytes: format_bytes(record.bytes),
                total: format_duration(record.total),
                queue: format_duration(record.queue),
                application: format_duration(record.application),
                worker: record
                    .worker
                    .map_or_else(|| "-".to_owned(), |worker| worker.to_string()),
                request_id: record.request_id.clone(),
            }
        })
        .collect();
    Data {
        app: inputs.app.clone(),
        release: inputs.release.load(Ordering::Relaxed),
        uptime: uptime(inputs.started.elapsed()),
        listen: inputs.listen.to_string(),
        https: inputs.https.map(|address| address.to_string()),
        summary,
        workers,
        requests,
        logs: inputs.logs.recent(LOG_LINES),
    }
}

fn uptime(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
