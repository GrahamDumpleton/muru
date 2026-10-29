//! The data behind the development dashboard (design specification section
//! 10.7): figures from the edge's access log and worker pools, and the
//! lines the host and workers wrote.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use muru_dashboard::{Data, History, MetricsSource, RequestRow, Sampler, Source, Summary, Worker};
use muru_edge::Pools;
use muru_edge::access::{AccessLog, RING_CAPACITY, Record, civil, format_bytes, format_duration};
use muru_edge::metrics::{aggregate, pool_samples, render};

use muru_controller::logs::Logs;

/// How far back the summary looks.
const WINDOW: Duration = Duration::from_secs(60);
/// Rows of recent requests shown.
const REQUEST_ROWS: usize = 50;
/// Log lines shown.
const LOG_LINES: usize = 200;
/// Open WebSockets listed.
const SOCKET_ROWS: usize = 200;

/// What the dashboard reads from.
pub struct Inputs {
    /// Whether this is a development run (section 13.1).
    pub development: bool,
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
    /// The listeners' connection counts (section 4.18).
    pub connections: muru_edge::ConnectionCounts,
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
    let pools = Arc::clone(&inputs.pools);
    let access_log = Arc::clone(&inputs.access_log);
    let connections = inputs.connections.clone();
    let metrics: MetricsSource = Arc::new(move || {
        let routing = pools.current();
        let samples = access_log
            .metrics()
            .samples()
            .into_iter()
            .chain(pool_samples(&routing))
            .chain(connections.samples());
        render(&aggregate(samples))
    });

    // Section 10.7: a point of history a second for the charts, from
    // the same counters the metrics show.

    let history = Arc::new(History::new());
    let recorder = Arc::clone(&history);
    let sampled_pools = Arc::clone(&inputs.pools);
    let sampled_log = Arc::clone(&inputs.access_log);
    tokio::spawn(async move {
        let mut sampler = Sampler::default();
        let mut ticks = tokio::time::interval(Duration::from_secs(1));

        loop {
            ticks.tick().await;
            let routing = sampled_pools.current();
            let samples: Vec<_> = sampled_log
                .metrics()
                .samples()
                .into_iter()
                .chain(pool_samples(&routing))
                .collect();
            let workers = routing.default_pool().workers().len() as u64;
            let reading = muru_controller::dashboard::reading(&samples, workers);
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());

            if let Some(point) = sampler.observe(now, reading) {
                recorder.record(point);
            }
        }
    });
    let source: Source = Arc::new(move || collect(&inputs));
    tokio::spawn(async move {
        if let Err(error) = muru_dashboard::serve(listener, source, metrics, history).await {
            tracing::warn!(%error, "the dashboard stopped");
        }
    });
    Ok(bound)
}

fn collect(inputs: &Inputs) -> Data {
    let now = SystemTime::now();

    // Rate and errors come from the per-second counters, which are exact
    // at any load. The record ring is bounded, so under load it reaches
    // back less than a minute: the percentiles and the request rows are
    // taken from whatever it covers.

    let minute = inputs.access_log.last_minute();
    let records = inputs.access_log.recent(RING_CAPACITY);
    let recent: Vec<&Record> = records
        .iter()
        .filter(|record| record.websocket.is_none())
        .filter(|record| {
            now.duration_since(record.started)
                .map_or(true, |age| age <= WINDOW)
        })
        .collect();
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
        .default_pool()
        .workers()
        .into_iter()
        .map(|worker| Worker {
            index: worker.index,
            link: worker.path.display().to_string(),
            slots: worker.slots,
            busy: worker.in_flight,
        })
        .collect();
    let websockets = muru_controller::dashboard::websockets_view(muru_edge::endpoints::report(
        &inputs.access_log,
        "",
        SOCKET_ROWS,
    ));

    // In flight is what the metric of that name counts, requests not
    // yet answered, with the WebSockets open beside it; a worker's busy
    // slots include a socket holding one, as muru_slots_busy does.

    let summary = Summary {
        rate: format!("{:.1}", minute.requests as f64 / minute.seconds as f64),
        p50: percentile(0.5),
        p95: percentile(0.95),
        errors: minute.errors,
        in_flight: inputs.access_log.metrics().in_flight(),
        websockets: websockets.open,
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
                reason: match &record.websocket {
                    Some(end) => format!("websocket closed by {}, code {}", end.by, end.code),
                    None => record.reason.clone(),
                },
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
        uptime: muru_dashboard::uptime(inputs.started.elapsed()),
        development: inputs.development,
        listen: inputs.listen.to_string(),
        https: inputs.https.map(|address| address.to_string()),
        summary,
        workers,
        requests,
        websockets,
        logs: inputs.logs.recent(LOG_LINES),
    }
}
