//! Prometheus metrics (design specification section 11.1): counters and
//! histograms of the requests the edge completed, gauges from its worker
//! pools, and the text exposition format. The edge reports its samples to
//! the daemon, which adds its own and serves them on `/metrics`; a
//! development run serves them from its dashboard.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

use muru_proto::edge::v1 as pb;

use crate::access::Record;
use crate::routing::Routing;

/// One sample: a metric name, its labels in order, and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// The metric name, with `_bucket`, `_sum` or `_count` for a
    /// histogram's samples.
    pub name: String,
    /// Label names and values, in the order they are written.
    pub labels: Vec<(String, String)>,
    /// The value.
    pub value: f64,
}

impl Sample {
    /// A sample with string labels.
    pub fn new(name: &str, labels: &[(&str, &str)], value: f64) -> Self {
        Self {
            name: name.to_owned(),
            labels: labels
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            value,
        }
    }

    /// The sample as the control protocol carries it.
    pub fn to_message(&self) -> pb::Sample {
        pb::Sample {
            name: self.name.clone(),
            labels: self
                .labels
                .iter()
                .map(|(name, value)| pb::Label {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            value: self.value,
        }
    }

    /// A sample from the control protocol.
    pub fn from_message(message: pb::Sample) -> Self {
        Self {
            name: message.name,
            labels: message
                .labels
                .into_iter()
                .map(|label| (label.name, label.value))
                .collect(),
            value: message.value,
        }
    }
}

/// What kind of metric a family is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Only ever goes up.
    Counter,
    /// A measurement of now.
    Gauge,
    /// Cumulative buckets with a sum and a count.
    Histogram,
}

/// A metric family: its name, kind and HELP text.
#[derive(Debug, Clone, Copy)]
pub struct Family {
    /// The name every sample of the family starts with.
    pub name: &'static str,
    /// Its kind, for the TYPE line.
    pub kind: Kind,
    /// Its HELP line.
    pub help: &'static str,
}

/// Every family, in the order the exposition lists them.
pub const FAMILIES: &[Family] = &[
    Family {
        name: "muru_requests_total",
        kind: Kind::Counter,
        help: "Requests completed, by status class, the reason when the edge generated the response, and the client class.",
    },
    Family {
        name: "muru_request_duration_seconds",
        kind: Kind::Histogram,
        help: "Time a request spent waiting for a worker (queue), in the application, and in total.",
    },
    Family {
        name: "muru_request_retries_total",
        kind: Kind::Counter,
        help: "Requests sent again to another worker after the first closed its connection before reading them.",
    },
    Family {
        name: "muru_requests_in_flight",
        kind: Kind::Gauge,
        help: "Requests the edge has taken and not yet answered; a WebSocket counts until its handshake is answered.",
    },
    Family {
        name: "muru_websocket_connections",
        kind: Kind::Gauge,
        help: "WebSockets open, by application, release, group and mount.",
    },
    Family {
        name: "muru_websockets_closed_total",
        kind: Kind::Counter,
        help: "WebSockets closed, by application, release, group and mount, which side closed, and how.",
    },
    Family {
        name: "muru_websocket_duration_seconds",
        kind: Kind::Histogram,
        help: "How long WebSockets stayed open, by application, release and group.",
    },
    Family {
        name: "muru_upload_spool_bytes",
        kind: Kind::Gauge,
        help: "Bytes of uploads the edge holds for requests in progress, by application.",
    },
    Family {
        name: "muru_rate_limited_total",
        kind: Kind::Counter,
        help: "Requests a rate limit refused, delayed or only logged, by limit, client class and action.",
    },
    Family {
        name: "muru_auth_requests_total",
        kind: Kind::Counter,
        help: "Authentication sub-requests, by result: allowed, cached, denied or unavailable.",
    },
    Family {
        name: "muru_queue_depth",
        kind: Kind::Gauge,
        help: "Requests waiting for a worker slot.",
    },
    Family {
        name: "muru_slots_busy",
        kind: Kind::Gauge,
        help: "Worker slots handling a request.",
    },
    Family {
        name: "muru_slots_total",
        kind: Kind::Gauge,
        help: "Worker slots across the workers: threads, or the event loop's concurrency.",
    },
    Family {
        name: "muru_upstream_health",
        kind: Kind::Gauge,
        help: "Whether an upstream address takes requests: 1 in service, 0 unhealthy or ejected.",
    },
    Family {
        name: "muru_workers_ready",
        kind: Kind::Gauge,
        help: "Workers listening for requests.",
    },
    Family {
        name: "muru_workers_desired",
        kind: Kind::Gauge,
        help: "Workers the autoscaler is bringing each group to, by application, release and group.",
    },
    Family {
        name: "muru_scaling_decisions_total",
        kind: Kind::Counter,
        help: "Autoscaler decisions, by application, release, group and direction.",
    },
    Family {
        name: "muru_churn_reaps_total",
        kind: Kind::Counter,
        help: "Workers the autoscaler reaped in the last hour, by application, release and group.",
    },
    Family {
        name: "muru_worker_requests_total",
        kind: Kind::Counter,
        help: "Requests a worker has served since it started, as it last reported.",
    },
    Family {
        name: "muru_worker_requests_in_flight",
        kind: Kind::Gauge,
        help: "Requests a worker was serving when it last reported.",
    },
    Family {
        name: "muru_worker_cpu_seconds_total",
        kind: Kind::Counter,
        help: "CPU time a worker has used, user and system, as it last reported.",
    },
    Family {
        name: "muru_worker_memory_bytes",
        kind: Kind::Gauge,
        help: "A worker's memory as it last reported it, by measure: rss, pss or uss.",
    },
    Family {
        name: "muru_connections_open",
        kind: Kind::Gauge,
        help: "Connections open on a listener.",
    },
    Family {
        name: "muru_connections_refused_total",
        kind: Kind::Counter,
        help: "Connections a listener refused, by reason: client_limit, overloaded (answered 503) or paused (closed).",
    },
    Family {
        name: "muru_snapshot_version",
        kind: Kind::Gauge,
        help: "The routing snapshot in force at the edge.",
    },
];

/// The histogram buckets in seconds, each with the `le` label it is
/// written with; the +Inf bucket is implied.
const BUCKETS: [(f64, &str); 13] = [
    (0.001, "0.001"),
    (0.0025, "0.0025"),
    (0.005, "0.005"),
    (0.01, "0.01"),
    (0.025, "0.025"),
    (0.05, "0.05"),
    (0.1, "0.1"),
    (0.25, "0.25"),
    (0.5, "0.5"),
    (1.0, "1"),
    (2.5, "2.5"),
    (5.0, "5"),
    (10.0, "10"),
];

/// The buckets of a WebSocket's lifetime, in seconds: from a second to a
/// day.
const LIFETIME_BUCKETS: [(f64, &str); 9] = [
    (1.0, "1"),
    (10.0, "10"),
    (60.0, "60"),
    (300.0, "300"),
    (900.0, "900"),
    (1800.0, "1800"),
    (3600.0, "3600"),
    (14400.0, "14400"),
    (86400.0, "86400"),
];

/// Cumulative counts per bucket, with the sum and count of what was
/// observed.
#[derive(Debug, Clone)]
struct Histogram {
    bounds: &'static [(f64, &'static str)],
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Default for Histogram {
    /// A histogram of request times.
    fn default() -> Self {
        Self::with(&BUCKETS)
    }
}

impl Histogram {
    fn with(bounds: &'static [(f64, &'static str)]) -> Self {
        Self {
            bounds,
            counts: vec![0; bounds.len()],
            sum: 0.0,
            count: 0,
        }
    }

    fn observe(&mut self, duration: Duration) {
        let seconds = duration.as_secs_f64();

        for (index, (bound, _)) in self.bounds.iter().enumerate() {
            if seconds <= *bound {
                self.counts[index] += 1;
            }
        }

        self.sum += seconds;
        self.count += 1;
    }

    fn samples(&self, name: &str, labels: &[(String, String)], into: &mut Vec<Sample>) {
        for (index, (_, le)) in self.bounds.iter().enumerate() {
            let mut with_le = labels.to_vec();
            with_le.push(("le".to_owned(), (*le).to_owned()));
            into.push(Sample {
                name: format!("{name}_bucket"),
                labels: with_le,
                value: self.counts[index] as f64,
            });
        }

        let mut with_le = labels.to_vec();
        with_le.push(("le".to_owned(), "+Inf".to_owned()));
        into.push(Sample {
            name: format!("{name}_bucket"),
            labels: with_le,
            value: self.count as f64,
        });
        into.push(Sample {
            name: format!("{name}_sum"),
            labels: labels.to_vec(),
            value: self.sum,
        });
        into.push(Sample {
            name: format!("{name}_count"),
            labels: labels.to_vec(),
            value: self.count as f64,
        });
    }
}

/// The labels of a completed request's counter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RequestKey {
    app: String,
    release: String,
    group: String,
    mount: String,
    status: String,
    reason: String,
    client_class: String,
}

/// The labels of a release's histograms and in-flight gauge.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReleaseKey {
    app: String,
    release: String,
}

impl ReleaseKey {
    fn labels(&self) -> Vec<(String, String)> {
        vec![
            ("app".to_owned(), self.app.clone()),
            ("release".to_owned(), self.release.clone()),
        ]
    }
}

/// The labels of a group's request histograms.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DurationKey {
    app: String,
    release: String,
    group: String,
}

impl DurationKey {
    fn labels(&self) -> Vec<(String, String)> {
        vec![
            ("app".to_owned(), self.app.clone()),
            ("release".to_owned(), self.release.clone()),
            ("group".to_owned(), self.group.clone()),
        ]
    }
}

/// The labels of a WebSocket's gauge: where it was routed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SocketKey {
    /// The application.
    pub app: String,
    /// The release.
    pub release: String,
    /// The worker group.
    pub group: String,
    /// The mount.
    pub mount: String,
}

impl SocketKey {
    fn labels(&self) -> Vec<(String, String)> {
        vec![
            ("app".to_owned(), self.app.clone()),
            ("release".to_owned(), self.release.clone()),
            ("group".to_owned(), self.group.clone()),
            ("mount".to_owned(), self.mount.clone()),
        ]
    }
}

/// A group's three request histograms.
#[derive(Debug, Default, Clone)]
struct Durations {
    queue: Histogram,
    application: Histogram,
    total: Histogram,
}

#[derive(Debug, Default)]
struct Inner {
    requests: BTreeMap<RequestKey, u64>,
    durations: BTreeMap<DurationKey, Durations>,
    /// Requests sent to a second worker (section 4.8), by group.
    retries: BTreeMap<DurationKey, u64>,
    in_flight: BTreeMap<ReleaseKey, i64>,
    /// WebSockets open, by where they were routed.
    websockets: BTreeMap<SocketKey, i64>,
    /// WebSockets closed, by where, which side closed and how.
    websockets_closed: BTreeMap<(SocketKey, &'static str, &'static str), u64>,
    /// How long WebSockets stayed open, by group.
    websocket_lifetimes: BTreeMap<DurationKey, Histogram>,
    /// Authentication sub-requests by result (section 4.16).
    auth: BTreeMap<String, u64>,
    rate_limited: BTreeMap<(String, String, String), u64>,
}

/// The request metrics of one edge, fed by the access log as each request
/// completes.
#[derive(Debug, Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    /// Count a request the edge has taken, until `finish` with the same
    /// labels.
    pub fn begin(&self, app: &str, release: &str) {
        let mut inner = self.inner.lock().expect("metrics poisoned");
        *inner
            .in_flight
            .entry(release_key(app, release))
            .or_default() += 1;
    }

    /// The request begun with these labels has been answered.
    pub fn finish(&self, app: &str, release: &str) {
        let mut inner = self.inner.lock().expect("metrics poisoned");
        *inner
            .in_flight
            .entry(release_key(app, release))
            .or_default() -= 1;
    }

    /// Requests taken and not yet answered, across every release: what
    /// `muru_requests_in_flight` sums to. A WebSocket counts until the
    /// application accepts it, then is open rather than in flight.
    pub fn in_flight(&self) -> usize {
        let inner = self.inner.lock().expect("metrics poisoned");
        inner
            .in_flight
            .values()
            .map(|count| usize::try_from(*count).unwrap_or(0))
            .sum()
    }

    /// Count a completed request and observe its timings. The queue and
    /// application times are observed only when a worker served it.
    pub fn observe(&self, record: &Record) {
        let mut inner = self.inner.lock().expect("metrics poisoned");
        let key = RequestKey {
            app: record.app.clone(),
            release: record.release.clone(),
            group: record.group.clone(),
            mount: record.mount.clone(),
            status: format!("{}xx", record.status / 100),
            reason: record.reason.clone(),
            client_class: record.client_class.clone(),
        };
        *inner.requests.entry(key).or_default() += 1;
        let by_group = DurationKey {
            app: record.app.clone(),
            release: record.release.clone(),
            group: record.group.clone(),
        };

        if record.retried {
            *inner.retries.entry(by_group.clone()).or_default() += 1;
        }

        let durations = inner.durations.entry(by_group).or_default();
        durations.total.observe(record.total);

        if record.worker.is_some() {
            durations.queue.observe(record.queue);
            durations.application.observe(record.application);
        }
    }

    /// Count a WebSocket as open, from its handshake until `close_websocket`.
    pub fn open_websocket(&self, key: &SocketKey) {
        let mut inner = self.inner.lock().expect("metrics poisoned");
        *inner.websockets.entry(key.clone()).or_default() += 1;
    }

    /// The WebSocket opened with this key has closed: `by` the client,
    /// the application or the edge, `how` as one of a fixed set of
    /// outcomes, after `lifetime`.
    pub fn close_websocket(
        &self,
        key: &SocketKey,
        by: &'static str,
        how: &'static str,
        lifetime: Duration,
    ) {
        let mut inner = self.inner.lock().expect("metrics poisoned");
        *inner.websockets.entry(key.clone()).or_default() -= 1;
        *inner
            .websockets_closed
            .entry((key.clone(), by, how))
            .or_default() += 1;
        inner
            .websocket_lifetimes
            .entry(DurationKey {
                app: key.app.clone(),
                release: key.release.clone(),
                group: key.group.clone(),
            })
            .or_insert_with(|| Histogram::with(&LIFETIME_BUCKETS))
            .observe(lifetime);
    }

    /// Count an authentication sub-request (section 4.16) by its result:
    /// allowed, cached, denied or unavailable.
    pub fn count_auth(&self, result: &str) {
        let mut inner = self.inner.lock().expect("metrics poisoned");
        *inner.auth.entry(result.to_owned()).or_default() += 1;
    }

    /// Count a rate limit's decision about a request (section 4.15):
    /// `rejected`, `delayed` or `logged`.
    pub fn count_rate_limit(&self, limit: &str, client_class: &str, action: &str) {
        let mut inner = self.inner.lock().expect("metrics poisoned");
        let key = (limit.to_owned(), client_class.to_owned(), action.to_owned());
        *inner.rate_limited.entry(key).or_default() += 1;
    }

    /// Every sample of the request metrics.
    pub fn samples(&self) -> Vec<Sample> {
        let inner = self.inner.lock().expect("metrics poisoned");
        let mut samples = Vec::new();

        for (key, count) in &inner.requests {
            samples.push(Sample {
                name: "muru_requests_total".to_owned(),
                labels: vec![
                    ("app".to_owned(), key.app.clone()),
                    ("release".to_owned(), key.release.clone()),
                    ("group".to_owned(), key.group.clone()),
                    ("mount".to_owned(), key.mount.clone()),
                    ("status".to_owned(), key.status.clone()),
                    ("reason".to_owned(), key.reason.clone()),
                    ("client_class".to_owned(), key.client_class.clone()),
                ],
                value: *count as f64,
            });
        }

        for (key, durations) in &inner.durations {
            for (phase, histogram) in [
                ("queue", &durations.queue),
                ("application", &durations.application),
                ("total", &durations.total),
            ] {
                let mut labels = key.labels();
                labels.push(("phase".to_owned(), phase.to_owned()));
                histogram.samples("muru_request_duration_seconds", &labels, &mut samples);
            }
        }

        for (key, count) in &inner.retries {
            samples.push(Sample {
                name: "muru_request_retries_total".to_owned(),
                labels: key.labels(),
                value: *count as f64,
            });
        }

        for (key, count) in &inner.in_flight {
            samples.push(Sample {
                name: "muru_requests_in_flight".to_owned(),
                labels: key.labels(),
                value: *count as f64,
            });
        }

        for (key, count) in &inner.websockets {
            samples.push(Sample {
                name: "muru_websocket_connections".to_owned(),
                labels: key.labels(),
                value: *count as f64,
            });
        }

        for ((key, by, how), count) in &inner.websockets_closed {
            let mut labels = key.labels();
            labels.push(("by".to_owned(), (*by).to_owned()));
            labels.push(("close".to_owned(), (*how).to_owned()));
            samples.push(Sample {
                name: "muru_websockets_closed_total".to_owned(),
                labels,
                value: *count as f64,
            });
        }

        for (key, histogram) in &inner.websocket_lifetimes {
            histogram.samples(
                "muru_websocket_duration_seconds",
                &key.labels(),
                &mut samples,
            );
        }

        for (result, count) in &inner.auth {
            samples.push(Sample {
                name: "muru_auth_requests_total".to_owned(),
                labels: vec![("result".to_owned(), result.clone())],
                value: *count as f64,
            });
        }

        for ((limit, client_class, action), count) in &inner.rate_limited {
            samples.push(Sample {
                name: "muru_rate_limited_total".to_owned(),
                labels: vec![
                    ("limit".to_owned(), limit.clone()),
                    ("client_class".to_owned(), client_class.clone()),
                    ("action".to_owned(), action.clone()),
                ],
                value: *count as f64,
            });
        }

        samples
    }
}

fn release_key(app: &str, release: &str) -> ReleaseKey {
    ReleaseKey {
        app: app.to_owned(),
        release: release.to_owned(),
    }
}

/// Gauges from the worker pools of the routing tables in force: slots
/// and queue depth per group, and the snapshot version.
pub fn pool_samples(routing: &Routing) -> Vec<Sample> {
    let mut samples = Vec::new();

    for pools in routing.app_pools() {
        let (app, release) = pools.labels();

        // A managed HTTP release (section 9.9) has instances, reported
        // by their health below, and no slots.

        if pools.server().is_some() {
            continue;
        }

        for (group, pool) in pools.pools() {
            let labels = [
                ("app", app),
                ("release", release),
                ("group", group.as_str()),
            ];
            let workers = pool.workers();
            samples.push(Sample::new(
                "muru_queue_depth",
                &labels,
                pool.waiting() as f64,
            ));
            samples.push(Sample::new(
                "muru_slots_busy",
                &labels,
                workers.iter().map(|worker| worker.in_flight).sum::<usize>() as f64,
            ));
            samples.push(Sample::new(
                "muru_slots_total",
                &labels,
                workers.iter().map(|worker| worker.slots).sum::<usize>() as f64,
            ));
        }
    }

    for upstream in routing.upstreams() {
        samples.extend(upstream.samples());
    }

    for server in routing.servers() {
        samples.extend(server.samples());
    }

    samples.push(Sample::new(
        "muru_snapshot_version",
        &[],
        routing.version() as f64,
    ));
    samples
}

/// Fold samples from several sources into one set: counters and gauges
/// with the same name and labels are summed, and the snapshot version
/// is the highest reported.
pub fn aggregate(samples: impl IntoIterator<Item = Sample>) -> Vec<Sample> {
    let mut folded: Vec<Sample> = Vec::new();

    for sample in samples {
        match folded
            .iter_mut()
            .find(|seen| seen.name == sample.name && seen.labels == sample.labels)
        {
            Some(seen) if sample.name == "muru_snapshot_version" => {
                seen.value = seen.value.max(sample.value);
            }
            Some(seen) => seen.value += sample.value,
            None => folded.push(sample),
        }
    }

    folded
}

/// The samples in the Prometheus text format, family by family in the
/// order of `FAMILIES`, with HELP and TYPE lines, sorted within a family
/// by labels with the buckets in order. Labels with empty values are
/// left out. Samples of no known family are not written.
pub fn render(samples: &[Sample]) -> String {
    let mut text = String::new();

    for family in FAMILIES {
        let mut members: Vec<&Sample> = samples
            .iter()
            .filter(|sample| belongs(sample, family))
            .collect();

        if members.is_empty() {
            continue;
        }

        members.sort_by(|left, right| order(left, right));
        let _ = writeln!(text, "# HELP {} {}", family.name, family.help);
        let _ = writeln!(text, "# TYPE {} {}", family.name, kind_name(family.kind));

        for sample in members {
            text.push_str(&sample.name);
            let labels: Vec<String> = sample
                .labels
                .iter()
                .filter(|(_, value)| !value.is_empty())
                .map(|(name, value)| format!("{name}=\"{}\"", escape(value)))
                .collect();

            if !labels.is_empty() {
                let _ = write!(text, "{{{}}}", labels.join(","));
            }

            let _ = writeln!(text, " {}", number(sample.value));
        }
    }

    text
}

fn belongs(sample: &Sample, family: &Family) -> bool {
    match family.kind {
        Kind::Histogram => sample
            .name
            .strip_prefix(family.name)
            .is_some_and(|suffix| matches!(suffix, "_bucket" | "_sum" | "_count")),
        Kind::Counter | Kind::Gauge => sample.name == family.name,
    }
}

/// Samples by name then labels, with `le` compared as a number so the
/// buckets stay in bound order.
fn order(left: &Sample, right: &Sample) -> std::cmp::Ordering {
    left.name.cmp(&right.name).then_with(|| {
        for ((left_name, left_value), (right_name, right_value)) in
            left.labels.iter().zip(&right.labels)
        {
            let ordering = left_name.cmp(right_name).then_with(|| {
                if left_name == "le" {
                    bound(left_value).total_cmp(&bound(right_value))
                } else {
                    left_value.cmp(right_value)
                }
            });

            if ordering.is_ne() {
                return ordering;
            }
        }

        left.labels.len().cmp(&right.labels.len())
    })
}

fn bound(le: &str) -> f64 {
    if le == "+Inf" {
        f64::INFINITY
    } else {
        le.parse().unwrap_or(f64::INFINITY)
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Counter => "counter",
        Kind::Gauge => "gauge",
        Kind::Histogram => "histogram",
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// A value as the text format writes it: whole numbers without a
/// fraction, others as the shortest representation.
fn number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::time::SystemTime;

    use super::*;

    fn record(status: u16, reason: &str, worker: Option<usize>, total_ms: u64) -> Record {
        Record {
            started: SystemTime::now(),
            request_id: "01".to_owned(),
            client: IpAddr::from([127, 0, 0, 1]),
            app: "shop".to_owned(),
            release: "r1".to_owned(),
            host: "shop.example.com".to_owned(),
            group: "default".to_owned(),
            mount: "default".to_owned(),
            snapshot: 3,
            method: "GET".to_owned(),
            target: "/".to_owned(),
            version: "HTTP/1.1".to_owned(),
            status,
            reason: reason.to_owned(),
            bytes: 10,
            worker,
            retried: false,
            queue: Duration::from_millis(1),
            application: Duration::from_millis(total_ms / 2),
            total: Duration::from_millis(total_ms),
            redirects: Vec::new(),
            client_class: "default".to_owned(),
            websocket: None,
        }
    }

    #[test]
    fn requests_are_counted_by_status_class_and_reason() {
        let metrics = Metrics::default();
        metrics.observe(&record(200, "", Some(0), 4));
        metrics.observe(&record(204, "", Some(1), 4));
        metrics.observe(&record(503, "no_workers", None, 0));
        let text = render(&metrics.samples());
        assert!(
            text.contains(
                "muru_requests_total{app=\"shop\",release=\"r1\",group=\"default\",mount=\"default\",status=\"2xx\",client_class=\"default\"} 2\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "muru_requests_total{app=\"shop\",release=\"r1\",group=\"default\",mount=\"default\",status=\"5xx\",reason=\"no_workers\",client_class=\"default\"} 1\n"
            ),
            "{text}"
        );
        assert!(text.starts_with("# HELP muru_requests_total "), "{text}");
        assert!(
            text.contains("# TYPE muru_requests_total counter\n"),
            "{text}"
        );
    }

    #[test]
    fn histograms_are_cumulative_and_skip_edge_generated_responses() {
        let metrics = Metrics::default();
        metrics.observe(&record(200, "", Some(0), 4));
        metrics.observe(&record(200, "", Some(0), 300));
        metrics.observe(&record(503, "no_workers", None, 0));
        let text = render(&metrics.samples());
        let line = |phase: &str, le: &str| {
            format!(
                "muru_request_duration_seconds_bucket{{app=\"shop\",release=\"r1\",group=\"default\",phase=\"{phase}\",le=\"{le}\"}}"
            )
        };
        assert!(
            text.contains(&format!("{} 2\n", line("total", "0.005"))),
            "{text}"
        );
        assert!(
            text.contains(&format!("{} 3\n", line("total", "0.5"))),
            "{text}"
        );
        assert!(
            text.contains(&format!("{} 3\n", line("total", "+Inf"))),
            "{text}"
        );
        assert!(
            text.contains(&format!("{} 2\n", line("queue", "+Inf"))),
            "{text}"
        );
        assert!(
            text.contains("muru_request_duration_seconds_count{app=\"shop\",release=\"r1\",group=\"default\",phase=\"application\"} 2\n"),
            "{text}"
        );

        // The buckets are written in bound order, not string order.

        let ten = text.find(&line("total", "10")).expect("10");
        let two_and_a_half = text.find(&line("total", "2.5")).expect("2.5");
        let inf = text.find(&line("total", "+Inf")).expect("+Inf");
        assert!(two_and_a_half < ten && ten < inf);
    }

    #[test]
    fn in_flight_follows_begin_and_finish() {
        let metrics = Metrics::default();
        metrics.begin("shop", "r1");
        metrics.begin("shop", "r1");
        metrics.finish("shop", "r1");
        let samples = metrics.samples();
        let in_flight = samples
            .iter()
            .find(|sample| sample.name == "muru_requests_in_flight")
            .expect("gauge");
        assert_eq!(in_flight.value, 1.0);
    }

    #[test]
    fn aggregation_sums_matching_samples_and_keeps_the_newest_snapshot() {
        let folded = aggregate([
            Sample::new("muru_slots_total", &[("app", "shop")], 4.0),
            Sample::new("muru_slots_total", &[("app", "shop")], 2.0),
            Sample::new("muru_slots_total", &[("app", "blog")], 1.0),
            Sample::new("muru_snapshot_version", &[], 3.0),
            Sample::new("muru_snapshot_version", &[], 2.0),
        ]);
        assert_eq!(folded.len(), 3);
        assert_eq!(folded[0].value, 6.0);
        assert_eq!(folded[1].value, 1.0);
        assert_eq!(folded[2].value, 3.0);
    }

    #[test]
    fn rendering_escapes_values_and_leaves_empty_labels_out() {
        let text = render(&[
            Sample::new(
                "muru_workers_ready",
                &[("app", "a\"b\\c"), ("group", "")],
                2.0,
            ),
            Sample::new(
                "muru_request_duration_seconds_sum",
                &[("app", "shop")],
                0.0025,
            ),
            Sample::new("not_a_family", &[], 1.0),
        ]);
        assert!(
            text.contains("muru_workers_ready{app=\"a\\\"b\\\\c\"} 2\n"),
            "{text}"
        );
        assert!(
            text.contains("muru_request_duration_seconds_sum{app=\"shop\"} 0.0025\n"),
            "{text}"
        );
        assert!(!text.contains("not_a_family"), "{text}");
    }
}
