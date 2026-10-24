//! The access log (design specification sections 11.2 and 13.1).
//!
//! Every request produces a record with its identifier, method, path,
//! status and reason, bytes sent, the worker that served it and timings:
//! queue (waiting for a worker connection), application (until the
//! response head arrived) and total, and the application, release, host,
//! group and mount that served it. Records are kept in a bounded ring in
//! memory for the dashboard and the CLI, and written to the terminal as
//! compact lines, to a file as JSON lines, only for errors and slow
//! requests, or not at all.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::metrics::Metrics;

/// How many records the in-memory ring keeps.
/// Records kept in memory for `recent()`, the dashboard and the CLI.
pub const RING_CAPACITY: usize = 1000;

/// Where access log lines go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// Compact lines on standard error.
    Terminal,
    /// Nothing is written; records are still kept in memory.
    Off,
    /// Compact lines on standard error for 4xx and 5xx responses and for
    /// requests slower than the threshold.
    Errors,
    /// JSON lines appended to a file.
    File(PathBuf),
}

/// Access log settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where lines go.
    pub destination: Destination,
    /// Requests at least this slow are shown with `Destination::Errors`.
    pub slow: Duration,
    /// Path patterns whose requests are not written (`*` matches anything,
    /// `?` one character); they are still kept in memory.
    pub exclude: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            destination: Destination::Terminal,
            slow: Duration::from_secs(1),
            exclude: Vec::new(),
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
    /// Time spent waiting for a worker connection.
    pub queue: Duration,
    /// Time from dispatch until the response head arrived.
    pub application: Duration,
    /// Time from arrival until the response was complete.
    pub total: Duration,
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
    config: Config,
    ring: Mutex<VecDeque<Record>>,
    minute: Mutex<[Bucket; MINUTE]>,
    metrics: Metrics,
    started: u64,
    file: Option<Mutex<File>>,
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
            config,
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            minute: Mutex::new([Bucket::default(); MINUTE]),
            metrics: Metrics::default(),
            started: unix_seconds(SystemTime::now()),
            file,
        })
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

    /// Keep a record and write it where the configuration says.
    pub fn record(&self, record: Record) {
        if self.shows(&record) {
            self.write(&record);
        }

        self.metrics.observe(&record);
        self.count(unix_seconds(SystemTime::now()), record.status);
        let mut ring = self.ring.lock().expect("access log ring poisoned");

        if ring.len() == RING_CAPACITY {
            ring.pop_front();
        }

        ring.push_back(record);
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
        let (Destination::File(path), Some(file)) = (&self.config.destination, &self.file) else {
            return Ok(());
        };

        let reopened = OpenOptions::new().create(true).append(true).open(path)?;
        *file.lock().expect("access log file poisoned") = reopened;
        Ok(())
    }

    /// The most recent records, oldest first.
    pub fn recent(&self, limit: usize) -> Vec<Record> {
        let ring = self.ring.lock().expect("access log ring poisoned");
        ring.iter().rev().take(limit).rev().cloned().collect()
    }

    fn shows(&self, record: &Record) -> bool {
        match self.config.destination {
            Destination::Off => return false,
            Destination::Errors if record.status < 400 && record.total < self.config.slow => {
                return false;
            }
            _ => {}
        }

        let path = record.path();
        !self
            .config
            .exclude
            .iter()
            .any(|pattern| glob_matches(pattern, path))
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
                let line = text_line(record);
                let stderr = io::stderr();
                let mut handle = stderr.lock();
                let _ = handle.write_all(line.as_bytes());
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

/// The compact terminal form: time, status, method, target, bytes and
/// timings, then the group and mount when named, the worker and request
/// id.
fn text_line(record: &Record) -> String {
    let mut line = String::with_capacity(160);
    let (_, hours, minutes, seconds, millis) = civil(record.started);
    let _ = write!(
        line,
        "{hours:02}:{minutes:02}:{seconds:02}.{millis:03} {} {} {} {} {}",
        record.status,
        record.method,
        record.target,
        format_bytes(record.bytes),
        format_duration(record.total)
    );

    if record.worker.is_some() {
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

    if !record.group.is_empty() && record.group != DEFAULT {
        let _ = write!(line, " group={}", record.group);
    }

    if !record.mount.is_empty() && record.mount != DEFAULT {
        let _ = write!(line, " mount={}", record.mount);
    }

    if let Some(worker) = record.worker {
        let _ = write!(line, " worker={worker}");
    }

    let _ = writeln!(line, " id={}", record.request_id);
    line
}

/// The structured form, one JSON object per line.
fn json_line(record: &Record) -> String {
    let (date, hours, minutes, seconds, millis) = civil(record.started);
    let mut line = String::with_capacity(320);
    let _ = write!(
        line,
        "{{\"time\":\"{date}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z\",\"request_id\":{},\"client\":{},\"app\":{},\"release\":{},\"host\":{},\"group\":{},\"mount\":{},\"method\":{},\"target\":{},\"version\":{},\"status\":{},\"reason\":{},\"bytes\":{},\"worker\":{},\"queue_ms\":{:.3},\"app_ms\":{:.3},\"total_ms\":{:.3},\"snapshot\":{}}}",
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
        millis_f64(record.queue),
        millis_f64(record.application),
        millis_f64(record.total),
        record.snapshot,
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
            slow: Duration::from_secs(1),
            exclude: Vec::new(),
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
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(20 * 1024), "20kB");
        assert_eq!(format_bytes(30 * 1024 * 1024), "30.0MB");
    }

    #[test]
    fn errors_mode_shows_failures_and_slow_requests() {
        let log = AccessLog::new(Config {
            destination: Destination::Errors,
            slow: Duration::from_millis(500),
            exclude: vec!["/health*".to_owned()],
        })
        .expect("no file to open");
        let record = |status: u16, target: &str, total: Duration| Record {
            started: SystemTime::now(),
            request_id: "id".to_owned(),
            client: "127.0.0.1".parse().expect("address"),
            app: String::new(),
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
            queue: Duration::ZERO,
            application: Duration::ZERO,
            total,
        };
        assert!(!log.shows(&record(200, "/", Duration::from_millis(10))));
        assert!(log.shows(&record(500, "/", Duration::from_millis(10))));
        assert!(log.shows(&record(200, "/", Duration::from_millis(900))));
        assert!(!log.shows(&record(500, "/healthz?x=1", Duration::from_millis(900))));
    }
}
