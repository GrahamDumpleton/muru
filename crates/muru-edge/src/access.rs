//! The access log (design specification sections 11.2 and 13.1).
//!
//! Every request produces a record with its identifier, method, path,
//! status and reason, bytes sent, the worker that served it and timings:
//! queue (waiting for a worker connection), application (until the
//! response head arrived) and total, and the application, release, host,
//! group and mount that served it. Records are kept in a bounded ring in
//! memory for the dashboard and the CLI, and written to the terminal as
//! compact lines or to a file as JSON lines. A filter says which are
//! written: all of them, only errors and slow requests, or none, less the
//! paths it leaves out. The node has one, an application may have its
//! own, and both change while the edge runs, from the daemon's snapshots.
//! An override changes what is written for a period, for one application
//! or every one and some paths or all, and lapses on its own.
//!
//! A WebSocket's request ends when the worker accepts it: its line is
//! written then, with the handshake's time. The open socket is then kept
//! in a list of its own, counted in its own metrics, and written as a
//! second line, with the same request id, when it closes.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::metrics::{Metrics, SocketKey};

/// How many records the in-memory ring keeps.
/// Records kept in memory for `recent()`, the dashboard and the CLI.
pub const RING_CAPACITY: usize = 1000;

/// How many lines a follower may fall behind before it skips some.
const FOLLOW_CAPACITY: usize = 1024;

/// Where access log lines go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// Compact lines on standard error.
    Terminal,
    /// JSON lines appended to a file.
    File(PathBuf),
    /// Nowhere, whatever the filter says; records are still counted and
    /// kept in memory.
    Off,
}

/// Which records are written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Every record.
    #[default]
    On,
    /// 4xx and 5xx responses, requests at least as slow as the threshold,
    /// and WebSockets that closed other than normally or going away.
    Errors,
    /// None; records are still counted and kept in memory.
    Off,
}

impl Mode {
    /// The mode a configuration names: `on`, `errors` or `off`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "on" => Some(Self::On),
            "errors" => Some(Self::Errors),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// The mode's name, as the configuration writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Errors => "errors",
            Self::Off => "off",
        }
    }
}

/// Which records are written: the mode, the threshold `errors` counts a
/// request as slow from, and the paths never written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// Which records are written.
    pub mode: Mode,
    /// Requests at least this slow are written under `Mode::Errors`.
    pub slow: Duration,
    /// Path patterns whose requests are not written (`*` matches anything,
    /// `?` one character); they are still kept in memory.
    pub exclude: Vec<String>,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            mode: Mode::On,
            slow: Duration::from_secs(1),
            exclude: Vec::new(),
        }
    }
}

impl Filter {
    /// The filter a snapshot carries.
    pub fn from_message(message: &muru_proto::edge::v1::AccessLog) -> Self {
        Self {
            mode: Mode::parse(&message.mode).unwrap_or_default(),
            slow: Duration::from_millis(message.slow_ms),
            exclude: message.exclude.clone(),
        }
    }

    /// The filter as a snapshot carries it.
    pub fn to_message(&self) -> muru_proto::edge::v1::AccessLog {
        muru_proto::edge::v1::AccessLog {
            mode: self.mode.as_str().to_owned(),
            slow_ms: u64::try_from(self.slow.as_millis()).unwrap_or(u64::MAX),
            exclude: self.exclude.clone(),
        }
    }

    /// Whether a record is written.
    fn shows(&self, record: &Record) -> bool {
        writes(self.mode, self.slow, &self.exclude, record)
    }
}

/// Whether a record is written under a mode, a slow threshold and the
/// paths left out.
fn writes(mode: Mode, slow: Duration, exclude: &[String], record: &Record) -> bool {
    // A WebSocket's close is shown with the errors when it ended other
    // than in order; its lifetime is not a slow request.

    match (mode, &record.websocket) {
        (Mode::Off, _) => return false,
        (Mode::Errors, Some(end)) if matches!(end.close, "normal" | "going_away") => {
            return false;
        }
        (Mode::Errors, None) if record.status < 400 && record.total < slow => {
            return false;
        }
        _ => {}
    }

    let path = record.path();
    !exclude.iter().any(|pattern| glob_matches(pattern, path))
}

/// A change to which records are written, for a period, over the
/// filters: for one application or every one, and for some paths or
/// every path. A record it covers is written by its mode, with the slow
/// threshold of the filter it replaces; one that names paths writes them
/// even where the filter leaves them out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    /// Which of the records it covers are written.
    pub mode: Mode,
    /// The application it covers, or every one when empty.
    pub app: String,
    /// The path patterns it covers, or every path when empty.
    pub paths: Vec<String>,
    /// When it ends.
    pub until: SystemTime,
}

impl Override {
    /// The override a request for one asks for, from now for `duration`:
    /// the mode is on, errors or off, and the period more than nothing.
    pub fn requested(
        mode: &str,
        app: &str,
        paths: &[String],
        duration: Duration,
    ) -> Result<Self, String> {
        let mode = Mode::parse(mode).ok_or_else(|| format!("{mode:?} is not on, errors or off"))?;

        if duration.is_zero() {
            return Err("a change to the access log lasts for a period: give --for".to_owned());
        }

        Ok(Self {
            mode,
            app: app.to_owned(),
            paths: paths.to_vec(),
            until: SystemTime::now() + duration,
        })
    }

    /// What it writes, as a phrase: "only errors and slow requests of
    /// shop to /api/*".
    pub fn describe(&self) -> String {
        let mut text = match self.mode {
            Mode::On => "every request",
            Mode::Errors => "only errors and slow requests",
            Mode::Off => "no request",
        }
        .to_owned();

        if !self.app.is_empty() {
            text.push_str(&format!(" of {}", self.app));
        }

        if !self.paths.is_empty() {
            text.push_str(&format!(" to {}", self.paths.join(", ")));
        }

        text
    }

    /// The override as the admin API reported it.
    pub fn from_admin_message(message: &muru_proto::admin::v1::AccessLogOverride) -> Self {
        Self {
            mode: Mode::parse(&message.mode).unwrap_or_default(),
            app: message.app.clone(),
            paths: message.paths.clone(),
            until: UNIX_EPOCH + Duration::from_millis(message.until_unix_ms),
        }
    }

    /// The override as the admin API reports it.
    pub fn to_admin_message(&self) -> muru_proto::admin::v1::AccessLogOverride {
        muru_proto::admin::v1::AccessLogOverride {
            mode: self.mode.as_str().to_owned(),
            app: self.app.clone(),
            paths: self.paths.clone(),
            until_unix_ms: self.until_unix_ms(),
        }
    }

    /// The override a message carries.
    pub fn from_message(message: &muru_proto::edge::v1::AccessLogOverride) -> Self {
        Self {
            mode: Mode::parse(&message.mode).unwrap_or_default(),
            app: message.app.clone(),
            paths: message.paths.clone(),
            until: UNIX_EPOCH + Duration::from_millis(message.until_unix_ms),
        }
    }

    /// The override as a snapshot carries it.
    pub fn to_message(&self) -> muru_proto::edge::v1::AccessLogOverride {
        muru_proto::edge::v1::AccessLogOverride {
            mode: self.mode.as_str().to_owned(),
            app: self.app.clone(),
            paths: self.paths.clone(),
            until_unix_ms: self.until_unix_ms(),
        }
    }

    /// When it ends, in Unix milliseconds.
    pub fn until_unix_ms(&self) -> u64 {
        self.until.duration_since(UNIX_EPOCH).map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
    }

    /// Whether it is still in force at `now`.
    pub fn in_force(&self, now: SystemTime) -> bool {
        now < self.until
    }

    /// Whether it covers a record of `app` at `path`.
    pub fn covers(&self, app: &str, path: &str) -> bool {
        (self.app.is_empty() || self.app == app)
            && (self.paths.is_empty()
                || self.paths.iter().any(|pattern| glob_matches(pattern, path)))
    }
}

/// The filters in force: the node's, and those of the applications that
/// have their own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    /// The filter for every application without its own.
    pub node: Filter,
    /// Applications' own filters, by name.
    pub apps: BTreeMap<String, Filter>,
    /// Changes for a period, the newest last; those past their end are
    /// ignored.
    pub overrides: Vec<Override>,
}

impl Filters {
    /// The filters with one for the node and none of the applications'
    /// own.
    pub fn node(filter: Filter) -> Self {
        Self {
            node: filter,
            apps: BTreeMap::new(),
            overrides: Vec::new(),
        }
    }

    /// The filters a snapshot carries, or none when it carries no
    /// choice, which leaves the current ones in force.
    pub fn from_snapshot(snapshot: &muru_proto::edge::v1::Snapshot) -> Option<Self> {
        let node = snapshot.access_log.as_ref()?;
        let apps = snapshot
            .apps
            .iter()
            .filter_map(|entry| {
                let filter = entry.access_log.as_ref()?;
                Some((entry.app.clone(), Filter::from_message(filter)))
            })
            .collect();

        Some(Self {
            node: Filter::from_message(node),
            apps,
            overrides: snapshot
                .access_log_overrides
                .iter()
                .map(Override::from_message)
                .collect(),
        })
    }

    /// The filter a record of `app` is written by, overrides aside.
    pub fn for_app(&self, app: &str) -> &Filter {
        self.apps.get(app).unwrap_or(&self.node)
    }

    /// Whether a record is written: by the newest override in force that
    /// covers it, else by its application's filter.
    fn shows(&self, record: &Record) -> bool {
        let filter = self.for_app(&record.app);

        if !self.overrides.is_empty() {
            let now = SystemTime::now();
            let path = record.path();
            let covering = self
                .overrides
                .iter()
                .rev()
                .find(|change| change.in_force(now) && change.covers(&record.app, path));

            if let Some(change) = covering {
                let exclude = if change.paths.is_empty() {
                    &filter.exclude[..]
                } else {
                    &[]
                };

                return writes(change.mode, filter.slow, exclude, record);
            }
        }

        filter.shows(record)
    }
}

/// Access log settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where lines go.
    pub destination: Destination,
    /// Which records are written until a snapshot says otherwise.
    pub filter: Filter,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            destination: Destination::Terminal,
            filter: Filter::default(),
        }
    }
}

/// One request, as logged.
#[derive(Debug, Clone)]
pub struct Record {
    /// When the request arrived.
    pub started: SystemTime,
    /// The request identifier (section 4.19).
    pub request_id: String,
    /// The client address.
    pub client: IpAddr,
    /// The application served, or empty when nothing names it.
    pub app: String,
    /// The release served, or empty when nothing names it.
    pub release: String,
    /// The virtual host that matched, else the host the client asked for.
    pub host: String,
    /// The worker group: `default`, or a named group's name.
    pub group: String,
    /// The mount: `default`, or a named mount's name.
    pub mount: String,
    /// The routing snapshot's version, 0 for a development run.
    pub snapshot: u64,
    /// The HTTP method.
    pub method: String,
    /// The path with its query string.
    pub target: String,
    /// The HTTP version, for example HTTP/1.1.
    pub version: String,
    /// The response status.
    pub status: u16,
    /// The Appendix C.3 reason when the edge generated the response.
    pub reason: String,
    /// Body bytes sent to the client.
    pub bytes: u64,
    /// The index of the worker that served the request, if one did.
    pub worker: Option<usize>,
    /// Section 4.8: the request was sent again to another worker after
    /// the first closed its connection before reading it.
    pub retried: bool,
    /// Time spent waiting for a worker connection.
    pub queue: Duration,
    /// Time from dispatch until the response head arrived.
    pub application: Duration,
    /// Time from arrival until the response was complete.
    pub total: Duration,
    /// The targets the request was redirected to internally (section
    /// 4.10), in order; empty when it was served where it arrived.
    pub redirects: Vec<String>,
    /// The client's class (section 4.14).
    pub client_class: String,
    /// How a WebSocket ended, on the record written when it closes, whose
    /// time is the close, whose total is how long it was open and whose
    /// bytes are those sent to the client; none on any other record.
    pub websocket: Option<WebSocketEnd>,
    /// The interface the worker handed the request to, `asgi` or `wsgi`
    /// (section 6.14); none for a request served where it arrived.
    pub handoff: Option<&'static str>,
}

/// How a WebSocket ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketEnd {
    /// Which side closed first: `client`, `application` or `edge`.
    pub by: &'static str,
    /// The close code.
    pub code: u16,
    /// The outcome, from a fixed set: normal, going_away, idle_timeout,
    /// drained, abnormal, too_large, protocol_error, server_error or other.
    pub close: &'static str,
    /// Messages received from the client.
    pub received: u64,
    /// Messages sent to the client.
    pub sent: u64,
    /// Bytes received from the client.
    pub received_bytes: u64,
}

/// The outcome of a WebSocket's close, for the metrics and the log, from
/// the side that closed first and its code.
pub fn close_outcome(by: &str, code: u16) -> &'static str {
    match (by, code) {
        (_, 1000 | 1005) => "normal",
        ("edge", 1001) => "idle_timeout",
        (_, 1001) => "going_away",
        (_, 1006) => "abnormal",
        (_, 1009) => "too_large",
        (_, 1002 | 1007) => "protocol_error",
        (_, 1011) => "server_error",
        _ => "other",
    }
}

/// A WebSocket open now, as the edge lists it.
#[derive(Debug, Clone)]
pub struct OpenWebSocket {
    /// The request id of its handshake.
    pub request_id: String,
    /// The client address.
    pub client: IpAddr,
    /// The application.
    pub app: String,
    /// The release.
    pub release: String,
    /// The virtual host.
    pub host: String,
    /// The worker group.
    pub group: String,
    /// The mount.
    pub mount: String,
    /// The path, without the query string.
    pub path: String,
    /// The worker it is held on.
    pub worker: Option<usize>,
    /// When the worker accepted it.
    pub opened: SystemTime,
    /// How long it has been open.
    pub age: Duration,
    /// Messages received from the client.
    pub received: u64,
    /// Messages sent to the client.
    pub sent: u64,
    /// Bytes received from the client.
    pub received_bytes: u64,
    /// Bytes sent to the client.
    pub sent_bytes: u64,
}

/// One open WebSocket's record and running counts.
#[derive(Debug)]
struct Socket {
    record: Record,
    opened: Instant,
    opened_at: SystemTime,
    received: AtomicU64,
    sent: AtomicU64,
    received_bytes: AtomicU64,
    sent_bytes: AtomicU64,
}

impl Socket {
    fn listed(&self) -> OpenWebSocket {
        OpenWebSocket {
            request_id: self.record.request_id.clone(),
            client: self.record.client,
            app: self.record.app.clone(),
            release: self.record.release.clone(),
            host: self.record.host.clone(),
            group: self.record.group.clone(),
            mount: self.record.mount.clone(),
            path: self.record.path().to_owned(),
            worker: self.record.worker,
            opened: self.opened_at,
            age: self.opened.elapsed(),
            received: self.received.load(Ordering::Relaxed),
            sent: self.sent.load(Ordering::Relaxed),
            received_bytes: self.received_bytes.load(Ordering::Relaxed),
            sent_bytes: self.sent_bytes.load(Ordering::Relaxed),
        }
    }

    fn key(&self) -> SocketKey {
        SocketKey {
            app: self.record.app.clone(),
            release: self.record.release.clone(),
            group: self.record.group.clone(),
            mount: self.record.mount.clone(),
        }
    }
}

/// An open WebSocket, listed and counted until it is closed or dropped;
/// dropped without `close`, it is recorded as closed abnormally by the
/// edge.
pub struct WebSocket {
    log: Arc<AccessLog>,
    id: u64,
    socket: Arc<Socket>,
    end: Option<(&'static str, u16, &'static str)>,
}

impl WebSocket {
    /// A message of `bytes` arrived from the client.
    pub fn received(&self, bytes: u64) {
        self.socket.received.fetch_add(1, Ordering::Relaxed);
        self.socket
            .received_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// A message of `bytes` went to the client.
    pub fn sent(&self, bytes: u64) {
        self.socket.sent.fetch_add(1, Ordering::Relaxed);
        self.socket.sent_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// The socket has closed, `by` the side that closed first with `code`.
    pub fn close(self, by: &'static str, code: u16) {
        self.close_as(by, code, close_outcome(by, code));
    }

    /// The socket has closed with an outcome its code does not tell, such
    /// as `drained` for one its worker closed on its way to stopping.
    pub fn close_as(mut self, by: &'static str, code: u16, close: &'static str) {
        self.end = Some((by, code, close));
    }
}

impl Drop for WebSocket {
    fn drop(&mut self) {
        let (by, code, close) = self.end.unwrap_or(("edge", 1006, "abnormal"));
        let socket = &self.socket;
        let lifetime = socket.opened.elapsed();
        self.log
            .websockets
            .lock()
            .expect("websockets poisoned")
            .remove(&self.id);
        self.log
            .metrics
            .close_websocket(&socket.key(), by, close, lifetime);

        let mut record = socket.record.clone();
        record.started = SystemTime::now();
        record.total = lifetime;
        record.bytes = socket.sent_bytes.load(Ordering::Relaxed);
        record.websocket = Some(WebSocketEnd {
            by,
            code,
            close,
            received: socket.received.load(Ordering::Relaxed),
            sent: socket.sent.load(Ordering::Relaxed),
            received_bytes: socket.received_bytes.load(Ordering::Relaxed),
        });
        self.log.keep(record);
    }
}

impl Record {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
}

/// Requests and 5xx responses completed within one second.
#[derive(Debug, Default, Clone, Copy)]
struct Bucket {
    second: u64,
    requests: u64,
    errors: u64,
}

/// Seconds of per-second counters kept.
const MINUTE: usize = 60;

/// Totals for the last minute, from the per-second counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Minute {
    /// Requests completed.
    pub requests: u64,
    /// Responses with a 5xx status.
    pub errors: u64,
    /// Seconds the totals cover: fewer than 60 while the log is younger
    /// than a minute, never fewer than 1.
    pub seconds: u64,
}

/// The access log: a ring of recent records, per-second counters for the
/// last minute, the request metrics, and the configured output.
pub struct AccessLog {
    destination: Destination,
    /// Which records are written, swapped whole by `set_filters`.
    filters: RwLock<Filters>,
    ring: Mutex<VecDeque<Record>>,
    minute: Mutex<[Bucket; MINUTE]>,
    metrics: Metrics,
    started: u64,
    file: Option<Mutex<File>>,
    /// The WebSockets open now, by a number of their own.
    websockets: Mutex<BTreeMap<u64, Arc<Socket>>>,
    next_websocket: AtomicU64,
    /// Each record as its JSON line, for a follower such as `muructl
    /// logs --access` under a development run (section 13.1).
    lines: tokio::sync::broadcast::Sender<String>,
}

/// A request counted as in flight until this is dropped.
pub struct InFlight {
    log: Arc<AccessLog>,
    app: String,
    release: String,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.log.metrics.finish(&self.app, &self.release);
    }
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

impl AccessLog {
    /// Prepare the log, opening the file when lines go to one.
    pub fn new(config: Config) -> io::Result<Self> {
        let file = match &config.destination {
            Destination::File(path) => Some(Mutex::new(
                OpenOptions::new().create(true).append(true).open(path)?,
            )),
            _ => None,
        };

        Ok(Self {
            destination: config.destination,
            filters: RwLock::new(Filters::node(config.filter)),
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            minute: Mutex::new([Bucket::default(); MINUTE]),
            metrics: Metrics::default(),
            started: unix_seconds(SystemTime::now()),
            file,
            websockets: Mutex::new(BTreeMap::new()),
            next_websocket: AtomicU64::new(0),
            lines: tokio::sync::broadcast::channel(FOLLOW_CAPACITY).0,
        })
    }

    /// The filters in force.
    pub fn filters(&self) -> Filters {
        self.filters
            .read()
            .expect("access log filters poisoned")
            .clone()
    }

    /// Put new filters in force: every record completed from now on is
    /// written by them.
    pub fn set_filters(&self, filters: Filters) {
        *self.filters.write().expect("access log filters poisoned") = filters;
    }

    /// Change the filters in force in place, as one step, and return
    /// what `change` does.
    pub fn update_filters<T>(&self, change: impl FnOnce(&mut Filters) -> T) -> T {
        change(&mut self.filters.write().expect("access log filters poisoned"))
    }

    /// The request metrics the records feed (section 11.1).
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Count a request as in flight for the application and release it
    /// is labelled with, until the returned guard is dropped.
    pub fn begin(self: &Arc<Self>, app: &str, release: &str) -> InFlight {
        self.metrics.begin(app, release);
        InFlight {
            log: Arc::clone(self),
            app: app.to_owned(),
            release: release.to_owned(),
        }
    }

    /// Count a redirect hop (section 4.10) in the metrics of the
    /// application that answered it, without a line of its own: the
    /// final response's record carries every hop.
    pub fn record_hop(&self, record: &Record) {
        self.metrics.observe(record);
    }

    /// Keep a record and write it where the configuration says.
    pub fn record(&self, record: Record) {
        self.metrics.observe(&record);
        self.count(unix_seconds(SystemTime::now()), record.status);
        self.keep(record);
    }

    /// Write a record where the configuration says and keep it in the
    /// ring, without counting it as a request.
    fn keep(&self, record: Record) {
        if self.shows(&record) {
            self.write(&record);
        }

        // A follower that is behind by more than the channel holds skips
        // the lines it missed; no follower, and nothing is sent.

        if self.lines.receiver_count() > 0 {
            let _ = self.lines.send(json_line(&record));
        }

        let mut ring = self.ring.lock().expect("access log ring poisoned");

        if ring.len() == RING_CAPACITY {
            ring.pop_front();
        }

        ring.push_back(record);
    }

    /// A WebSocket's handshake, already recorded as its request, was
    /// accepted: list and count the socket until the returned guard is
    /// closed or dropped.
    pub fn open_websocket(self: &Arc<Self>, record: Record) -> WebSocket {
        let socket = Arc::new(Socket {
            record,
            opened: Instant::now(),
            opened_at: SystemTime::now(),
            received: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            received_bytes: AtomicU64::new(0),
            sent_bytes: AtomicU64::new(0),
        });
        let id = self.next_websocket.fetch_add(1, Ordering::Relaxed);
        self.metrics.open_websocket(&socket.key());
        self.websockets
            .lock()
            .expect("websockets poisoned")
            .insert(id, Arc::clone(&socket));
        WebSocket {
            log: Arc::clone(self),
            id,
            socket,
            end: None,
        }
    }

    /// The WebSockets open now, the longest open first.
    pub fn websockets(&self) -> Vec<OpenWebSocket> {
        self.websockets
            .lock()
            .expect("websockets poisoned")
            .values()
            .map(|socket| socket.listed())
            .collect()
    }

    /// How many WebSockets are open, by application, release and group.
    pub fn websocket_counts(&self) -> BTreeMap<(String, String, String), usize> {
        let mut counts = BTreeMap::new();

        for socket in self
            .websockets
            .lock()
            .expect("websockets poisoned")
            .values()
        {
            let record = &socket.record;
            *counts
                .entry((
                    record.app.clone(),
                    record.release.clone(),
                    record.group.clone(),
                ))
                .or_default() += 1;
        }

        counts
    }

    fn count(&self, second: u64, status: u16) {
        let mut minute = self.minute.lock().expect("access log counters poisoned");
        let bucket = &mut minute[(second % MINUTE as u64) as usize];

        if bucket.second != second {
            *bucket = Bucket {
                second,
                requests: 0,
                errors: 0,
            };
        }

        bucket.requests += 1;

        if status >= 500 {
            bucket.errors += 1;
        }
    }

    /// Requests and errors over the last minute. Unlike `recent()`, this
    /// is exact at any rate: the counters are per second, not per record.
    pub fn last_minute(&self) -> Minute {
        self.minute_at(unix_seconds(SystemTime::now()))
    }

    fn minute_at(&self, now: u64) -> Minute {
        let since = now.saturating_sub(MINUTE as u64 - 1);
        let minute = self.minute.lock().expect("access log counters poisoned");
        let (requests, errors) = minute
            .iter()
            .filter(|bucket| bucket.second >= since && bucket.second <= now)
            .fold((0, 0), |(requests, errors), bucket| {
                (requests + bucket.requests, errors + bucket.errors)
            });
        Minute {
            requests,
            errors,
            seconds: (now.saturating_sub(self.started) + 1).clamp(1, MINUTE as u64),
        }
    }

    /// Open the file again by name, for section 11.3: after an external
    /// rotation has renamed the file, lines go to a new one at the path.
    /// Nothing happens when lines do not go to a file.
    pub fn reopen(&self) -> io::Result<()> {
        let (Destination::File(path), Some(file)) = (&self.destination, &self.file) else {
            return Ok(());
        };

        let reopened = OpenOptions::new().create(true).append(true).open(path)?;
        *file.lock().expect("access log file poisoned") = reopened;
        Ok(())
    }

    /// The most recent records as JSON lines, oldest first.
    pub fn recent_lines(&self, limit: usize) -> Vec<String> {
        self.recent(limit).iter().map(json_line).collect()
    }

    /// Every record from now on as its JSON line, for a follower.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.lines.subscribe()
    }

    /// The most recent records, oldest first.
    pub fn recent(&self, limit: usize) -> Vec<Record> {
        let ring = self.ring.lock().expect("access log ring poisoned");
        ring.iter().rev().take(limit).rev().cloned().collect()
    }

    fn shows(&self, record: &Record) -> bool {
        if self.destination == Destination::Off {
            return false;
        }

        self.filters
            .read()
            .expect("access log filters poisoned")
            .shows(record)
    }

    fn write(&self, record: &Record) {
        match &self.file {
            Some(file) => {
                let line = json_line(record);
                let mut file = file.lock().expect("access log file poisoned");

                if file.write_all(line.as_bytes()).is_err() {
                    // Section 11.2: a stalled log never holds up requests.
                }
            }
            None => {
                // Section 11.8: the terminal's compact line, or under
                // muructl serve --log-format json the record itself.

                let line = if muru_common::log::json() {
                    muru_common::log::with_identity(json_line(record).replacen(
                        '{',
                        "{\"kind\":\"access\",",
                        1,
                    ))
                } else {
                    text_line(record)
                };
                muru_common::log::write(muru_common::log::Stream::Stderr, line.as_bytes());
            }
        }
    }
}

/// Whether a path matches a pattern where `*` matches any run of characters
/// and `?` exactly one.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    fn step(pattern: &[char], path: &[char]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some(('*', rest)) => (0..=path.len()).any(|skip| step(rest, &path[skip..])),
            Some(('?', rest)) => !path.is_empty() && step(rest, &path[1..]),
            Some((literal, rest)) => path.first() == Some(literal) && step(rest, &path[1..]),
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let path: Vec<char> = path.chars().collect();
    step(&pattern, &path)
}

/// The group and mount name left out of the compact form.
const DEFAULT: &str = "default";

/// The compact form: the time (section 11.8), status, method, target, bytes and
/// timings, then the group and mount when named, the worker and request
/// id.
fn text_line(record: &Record) -> String {
    let mut line = String::with_capacity(160);
    let time = muru_common::log::record_time(muru_common::log::Stream::Stderr, record.started);

    let total = if record.websocket.is_some() {
        format_lifetime(record.total)
    } else {
        format_duration(record.total)
    };
    let _ = write!(
        line,
        "{time} {} {} {} {} {total}",
        record.status,
        record.method,
        record.target,
        format_bytes(record.bytes),
    );

    if let Some(end) = &record.websocket {
        let _ = write!(
            line,
            " websocket=closed by={} code={} received={} sent={}",
            end.by, end.code, end.received, end.sent
        );
    } else if record.worker.is_some() {
        let _ = write!(
            line,
            " queue={} app={}",
            format_duration(record.queue),
            format_duration(record.application)
        );
    }

    if !record.reason.is_empty() {
        let _ = write!(line, " reason={}", record.reason);
    }

    if !record.redirects.is_empty() {
        let _ = write!(line, " redirects={}", record.redirects.join(","));
    }

    if !record.group.is_empty() && record.group != DEFAULT {
        let _ = write!(line, " group={}", record.group);
    }

    if !record.mount.is_empty() && record.mount != DEFAULT {
        let _ = write!(line, " mount={}", record.mount);
    }

    if !record.client_class.is_empty() && record.client_class != DEFAULT {
        let _ = write!(line, " class={}", record.client_class);
    }

    if let Some(worker) = record.worker {
        let _ = write!(line, " worker={worker}");
    }

    if record.retried {
        line.push_str(" retried=1");
    }

    if let Some(handoff) = record.handoff {
        let _ = write!(line, " handoff={handoff}");
    }

    let _ = writeln!(line, " id={}", record.request_id);
    line
}

/// The structured form, one JSON object per line.
pub fn json_line(record: &Record) -> String {
    let (date, hours, minutes, seconds, millis) = civil(record.started);
    let mut line = String::with_capacity(320);
    let _ = write!(
        line,
        "{{\"time\":\"{date}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z\",\"request_id\":{},\"client\":{},\"app\":{},\"release\":{},\"host\":{},\"group\":{},\"mount\":{},\"method\":{},\"target\":{},\"version\":{},\"status\":{},\"reason\":{},\"bytes\":{},\"worker\":{},\"retried\":{},\"queue_ms\":{:.3},\"app_ms\":{:.3},\"total_ms\":{:.3},\"snapshot\":{},\"redirects\":[{}],\"client_class\":{},\"websocket\":{},\"handoff\":{}}}",
        json_string(&record.request_id),
        json_string(&record.client.to_string()),
        json_string(&record.app),
        json_string(&record.release),
        json_string(&record.host),
        json_string(&record.group),
        json_string(&record.mount),
        json_string(&record.method),
        json_string(&record.target),
        json_string(&record.version),
        record.status,
        json_string(&record.reason),
        record.bytes,
        record
            .worker
            .map_or_else(|| "null".to_owned(), |worker| worker.to_string()),
        record.retried,
        millis_f64(record.queue),
        millis_f64(record.application),
        millis_f64(record.total),
        record.snapshot,
        record
            .redirects
            .iter()
            .map(|target| json_string(target))
            .collect::<Vec<_>>()
            .join(","),
        json_string(&record.client_class),
        record.websocket.as_ref().map_or_else(
            || "null".to_owned(),
            |end| format!(
                "{{\"event\":\"closed\",\"by\":{},\"code\":{},\"close\":{},\"received\":{},\"sent\":{},\"received_bytes\":{}}}",
                json_string(end.by),
                end.code,
                json_string(end.close),
                end.received,
                end.sent,
                end.received_bytes
            )
        ),
        record
            .handoff
            .map_or_else(|| "null".to_owned(), json_string),
    );
    line.push('\n');
    line
}

fn millis_f64(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Format a duration briefly: 0.9ms, 42ms, 1.50s.
pub fn format_duration(duration: Duration) -> String {
    let millis = millis_f64(duration);

    if millis >= 1000.0 {
        format!("{:.2}s", millis / 1000.0)
    } else if millis >= 10.0 {
        format!("{millis:.0}ms")
    } else {
        format!("{millis:.1}ms")
    }
}

/// Format how long something lasted that may run to hours, such as a
/// WebSocket: as `format_duration` under a minute, then 5m03s or 2h04m.
pub fn format_lifetime(duration: Duration) -> String {
    let seconds = duration.as_secs();

    if seconds < 60 {
        format_duration(duration)
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// Format a byte count briefly: 512B, 20kB, 30.0MB.
pub fn format_bytes(count: u64) -> String {
    if count >= 10 * 1024 * 1024 {
        format!("{:.1}MB", count as f64 / (1024.0 * 1024.0))
    } else if count >= 10 * 1024 {
        format!("{:.0}kB", count as f64 / 1024.0)
    } else {
        format!("{count}B")
    }
}

/// The UTC date as YYYY-MM-DD, then hours, minutes, seconds and milliseconds.
pub fn civil(time: SystemTime) -> (String, u64, u64, u64, u64) {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs();
    let millis = u64::from(since_epoch.subsec_millis());
    let days = seconds / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    (
        format!("{year:04}-{month:02}-{day:02}"),
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
        millis,
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');

    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }

    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_second_counters_sum_over_the_last_minute() {
        let log = AccessLog::new(Config {
            destination: Destination::Off,
            ..Config::default()
        })
        .expect("log");

        // Counted in time order, as they arrive: the second a minute before
        // `now` shares a bucket with `now` and is overwritten by it.

        let now = log.started + 10;
        log.count(now - 60, 200);
        log.count(now - 59, 500);
        log.count(now, 200);
        log.count(now, 503);
        let minute = log.minute_at(now);
        assert_eq!(minute.requests, 3, "the record a minute old has aged out");
        assert_eq!(minute.errors, 2);
        assert_eq!(minute.seconds, 11, "the log is eleven seconds old");
        assert_eq!(log.minute_at(log.started + 200).seconds, 60);
        assert_eq!(log.minute_at(log.started + 200).requests, 0);
    }

    #[test]
    fn globs() {
        assert!(glob_matches("/static/*", "/static/app.css"));
        assert!(glob_matches("/static/*", "/static/"));
        assert!(!glob_matches("/static/*", "/other"));
        assert!(glob_matches("*.png", "/images/logo.png"));
        assert!(glob_matches("/healthz", "/healthz"));
        assert!(!glob_matches("/healthz", "/healthz/x"));
        assert!(glob_matches("/item/?", "/item/7"));
        assert!(!glob_matches("/item/?", "/item/77"));
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        assert_eq!(civil_from_days(20_750), (2026, 10, 24));
    }

    #[test]
    fn units() {
        assert_eq!(format_duration(Duration::from_micros(870)), "0.9ms");
        assert_eq!(format_duration(Duration::from_millis(42)), "42ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.50s");
        assert_eq!(format_lifetime(Duration::from_millis(1500)), "1.50s");
        assert_eq!(format_lifetime(Duration::from_secs(303)), "5m03s");
        assert_eq!(format_lifetime(Duration::from_secs(7440)), "2h04m");
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(20 * 1024), "20kB");
        assert_eq!(format_bytes(30 * 1024 * 1024), "30.0MB");
    }

    fn record(app: &str, status: u16, target: &str, total: Duration) -> Record {
        Record {
            started: SystemTime::now(),
            request_id: "id".to_owned(),
            client: "127.0.0.1".parse().expect("address"),
            app: app.to_owned(),
            release: String::new(),
            host: String::new(),
            group: DEFAULT.to_owned(),
            mount: DEFAULT.to_owned(),
            snapshot: 0,
            method: "GET".to_owned(),
            target: target.to_owned(),
            version: "HTTP/1.1".to_owned(),
            status,
            reason: String::new(),
            bytes: 0,
            worker: Some(0),
            retried: false,
            queue: Duration::ZERO,
            application: Duration::ZERO,
            total,
            redirects: Vec::new(),
            client_class: "default".to_owned(),
            websocket: None,
            handoff: None,
        }
    }

    #[test]
    fn errors_mode_shows_failures_and_slow_requests() {
        let log = AccessLog::new(Config {
            destination: Destination::Terminal,
            filter: Filter {
                mode: Mode::Errors,
                slow: Duration::from_millis(500),
                exclude: vec!["/health*".to_owned()],
            },
        })
        .expect("no file to open");
        let quick = Duration::from_millis(10);
        let slow = Duration::from_millis(900);
        assert!(!log.shows(&record("", 200, "/", quick)));
        assert!(log.shows(&record("", 500, "/", quick)));
        assert!(log.shows(&record("", 200, "/", slow)));
        assert!(!log.shows(&record("", 500, "/healthz?x=1", slow)));
    }

    #[test]
    fn filters_are_swapped_whole_and_an_application_may_have_its_own() {
        let log = AccessLog::new(Config::default()).expect("no file to open");
        let quick = Duration::from_millis(10);
        assert!(log.shows(&record("shop", 200, "/", quick)));

        // The node's filter turns lines off, and one application keeps
        // its errors.

        let errors = Filter {
            mode: Mode::Errors,
            ..Filter::default()
        };
        let mut filters = Filters::node(Filter {
            mode: Mode::Off,
            ..Filter::default()
        });
        filters.apps.insert("shop".to_owned(), errors.clone());
        log.set_filters(filters.clone());
        assert_eq!(log.filters(), filters);
        assert!(!log.shows(&record("blog", 500, "/", quick)));
        assert!(!log.shows(&record("shop", 200, "/", quick)));
        assert!(log.shows(&record("shop", 500, "/", quick)));

        // A snapshot carries the same, and one without a choice changes
        // nothing.

        let snapshot = muru_proto::edge::v1::Snapshot {
            access_log: Some(filters.node.to_message()),
            apps: vec![muru_proto::edge::v1::AppWorkers {
                app: "shop".to_owned(),
                access_log: Some(errors.to_message()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(Filters::from_snapshot(&snapshot), Some(filters));
        assert_eq!(
            Filters::from_snapshot(&muru_proto::edge::v1::Snapshot::default()),
            None
        );
    }

    #[test]
    fn the_newest_override_in_force_that_covers_a_record_decides() {
        let log = AccessLog::new(Config {
            destination: Destination::Terminal,
            filter: Filter {
                mode: Mode::Errors,
                exclude: vec!["/api/health".to_owned()],
                ..Filter::default()
            },
        })
        .expect("no file to open");
        let quick = Duration::from_millis(10);
        let later = SystemTime::now() + Duration::from_secs(60);
        let mut filters = log.filters();

        // Every request of shop's API, the excluded probe among them,
        // and nothing at all of blog's for a while; then a change that
        // has already ended, which is ignored.

        filters.overrides = vec![
            Override {
                mode: Mode::On,
                app: "shop".to_owned(),
                paths: vec!["/api/*".to_owned()],
                until: later,
            },
            Override {
                mode: Mode::Off,
                app: "blog".to_owned(),
                paths: Vec::new(),
                until: later,
            },
            Override {
                mode: Mode::Off,
                app: String::new(),
                paths: Vec::new(),
                until: SystemTime::now() - Duration::from_secs(1),
            },
        ];
        log.set_filters(filters.clone());
        assert!(log.shows(&record("shop", 200, "/api/items", quick)));
        assert!(log.shows(&record("shop", 200, "/api/health", quick)));
        assert!(!log.shows(&record("shop", 200, "/home", quick)));
        assert!(log.shows(&record("shop", 500, "/home", quick)));
        assert!(!log.shows(&record("blog", 500, "/", quick)));

        // The newest decides where two cover a record.

        filters.overrides.push(Override {
            mode: Mode::Errors,
            app: String::new(),
            paths: vec!["/api/*".to_owned()],
            until: later,
        });
        log.set_filters(filters.clone());
        assert!(!log.shows(&record("shop", 200, "/api/items", quick)));

        assert_eq!(
            filters.overrides[0].describe(),
            "every request of shop to /api/*"
        );
        assert!(Override::requested("loud", "", &[], Duration::from_secs(1)).is_err());
        assert!(Override::requested("on", "", &[], Duration::ZERO).is_err());

        let message = filters.overrides[0].to_message();
        assert_eq!(message.mode, "on");
        let back = Override::from_message(&message);
        assert_eq!(back.until_unix_ms(), filters.overrides[0].until_unix_ms());
    }
}
