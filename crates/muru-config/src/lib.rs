//! Configuration schema types, TOML parsing, validation, precedence merge and JSON Schema output.
//!
//! Every user-visible setting is defined once, here, and the command line,
//! configuration files and later the admin API all map onto these types
//! (design specification sections 3.3 and 14.4). Phase 1 covers the
//! application-shipped defaults of section 6.8: the `[tool.muru]` table in a
//! project's pyproject.toml, or the same keys at the top of a muru.toml.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The interface an entrypoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Interface {
    /// Detect from the callable: a coroutine function or an object with an
    /// `async def __call__` is ASGI, anything else WSGI.
    Auto,
    /// PEP 3333.
    Wsgi,
    /// ASGI 3.
    Asgi,
}

/// Whether the application is imported in the host before forking (section 6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preload {
    /// Preload when the import check passes, otherwise import per worker.
    Auto,
    /// Preload, and fail the release when the import check does not pass.
    True,
    /// Import in every worker.
    False,
}

/// When a factory entrypoint is called (section 6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactoryPhase {
    /// In each worker, after the post_fork hook.
    Worker,
    /// Once in the host, during preload.
    Host,
}

macro_rules! words {
    ($type:ident { $($variant:ident => $word:literal),+ $(,)? }) => {
        impl $type {
            /// The setting's value as it is written.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $word,)+
                }
            }

            /// Every accepted value.
            pub const ALL: &'static [&'static str] = &[$($word),+];
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $type {
            type Err = ParseError;

            fn from_str(text: &str) -> Result<Self, ParseError> {
                match text {
                    $($word => Ok(Self::$variant),)+
                    _ => Err(ParseError {
                        setting: stringify!($type),
                        value: text.to_owned(),
                        accepted: Self::ALL,
                    }),
                }
            }
        }
    };
}

words!(Interface { Auto => "auto", Wsgi => "wsgi", Asgi => "asgi" });
words!(Preload { Auto => "auto", True => "true", False => "false" });
words!(FactoryPhase { Worker => "worker", Host => "host" });

/// A Python logging level (section 11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    /// Everything.
    Debug,
    /// Informational messages and above.
    Info,
    /// Python's default: warnings and above.
    Warning,
    /// Errors and above.
    Error,
    /// Only critical messages.
    Critical,
}

words!(LogLevel {
    Debug => "DEBUG",
    Info => "INFO",
    Warning => "WARNING",
    Error => "ERROR",
    Critical => "CRITICAL",
});

/// A value that is not one of a setting's accepted words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not a {setting}: expected one of {}", accepted.join(", "))]
pub struct ParseError {
    /// The setting's type.
    pub setting: &'static str,
    /// What was given.
    pub value: String,
    /// What is accepted.
    pub accepted: &'static [&'static str],
}

/// Parse a size such as 10MB, 512KB, 1GB or a plain number of bytes.
/// KB, MB and GB are powers of 1024, as mod_wsgi counts them (ADR 0008);
/// case does not matter and a lone K, M or G is accepted.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let upper = text.to_ascii_uppercase();
    let (number, scale) = if let Some(number) = upper.strip_suffix("GB").or(upper.strip_suffix('G'))
    {
        (number, 1024u64 * 1024 * 1024)
    } else if let Some(number) = upper.strip_suffix("MB").or(upper.strip_suffix('M')) {
        (number, 1024 * 1024)
    } else if let Some(number) = upper.strip_suffix("KB").or(upper.strip_suffix('K')) {
        (number, 1024)
    } else if let Some(number) = upper.strip_suffix('B') {
        (number, 1)
    } else {
        (upper.as_str(), 1)
    };
    let value: u64 = number
        .trim()
        .parse()
        .map_err(|_| format!("{text:?} is not a size such as 10MB or 512KB"))?;
    value
        .checked_mul(scale)
        .ok_or_else(|| format!("{text:?} is too large"))
}

/// Write a size the way the configuration accepts it: the largest unit
/// that divides it exactly, else bytes.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "KB")];

    for (scale, unit) in UNITS {
        if bytes != 0 && bytes.is_multiple_of(scale) {
            return format!("{}{unit}", bytes / scale);
        }
    }

    bytes.to_string()
}

/// Parse a duration such as 250ms, 1.5s, 2m, 1h or 2 (seconds), as the
/// configuration and the command line write them.
pub fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
    let text = text.trim();

    let (number, scale) = if let Some(number) = text.strip_suffix("ms") {
        (number, 0.001)
    } else if let Some(number) = text.strip_suffix('h') {
        (number, 3600.0)
    } else if let Some(number) = text.strip_suffix('m') {
        (number, 60.0)
    } else if let Some(number) = text.strip_suffix('s') {
        (number, 1.0)
    } else {
        (text, 1.0)
    };

    let value: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("{text:?} is not a duration, for example 250ms, 1.5s or 2m"))?;

    if value < 0.0 || !value.is_finite() {
        return Err(format!("{text:?} is not a duration"));
    }

    Ok(std::time::Duration::from_secs_f64(value * scale))
}

/// Write a duration the way the configuration accepts it: whole seconds
/// as `5s`, otherwise milliseconds as `250ms`.
pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();

    if millis != 0 && millis.is_multiple_of(1000) {
        format!("{}s", millis / 1000)
    } else {
        format!("{millis}ms")
    }
}

/// A mount, the `[app.NAME.release.ID.mounts.NAME]` or
/// `[tool.muru.mounts.NAME]` table (section 6.7): a path served by another
/// entrypoint in the same workers. The release's own entrypoint is the
/// default mount at /.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// The path prefix; the longest wins, on segment boundaries.
    pub path: String,
    /// The application as module:callable.
    pub entrypoint: String,
    /// The interface, when declared rather than detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<Interface>,
}

/// A worker group, the `[app.NAME.release.ID.groups.NAME]` or
/// `[tool.muru.groups.NAME]` table (section 7.1): the paths it serves
/// and its own sizes. Paths not claimed by a group go to the default
/// group, which is the release's own settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// Path prefixes, with an optional trailing `*`; the most specific
    /// match across all groups wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Worker processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<usize>,
    /// Application threads per worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<usize>,
    /// Threads for synchronous code called from an ASGI application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_threads: Option<usize>,
    /// Requests an ASGI worker's event loop takes at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
}

/// A release definition, the `[app.NAME.release.ID]` table (appendix C):
/// the keys a deploy takes. `muructl run --print-config` writes one so a
/// laptop setup can become a declared release, and `muructl apply` reads
/// them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// The source directory. A relative path is taken from the file's
    /// directory. Required by `muructl apply`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The application as module:callable, else from the source's
    /// `[tool.muru]` table.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub entrypoint: String,
    /// The interface, when declared rather than detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<Interface>,
    /// Whether the entrypoint is a factory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory: Option<bool>,
    /// When a factory is called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory_phase: Option<FactoryPhase>,
    /// The interpreter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// Worker processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<usize>,
    /// Application threads per worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<usize>,
    /// Threads for synchronous code called from an ASGI application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_threads: Option<usize>,
    /// Requests an ASGI worker's event loop takes at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// The preload mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preload: Option<Preload>,
    /// The hooks module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<String>,
    /// Worker groups by name (section 7.1).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Group>,
    /// Mounts by name (section 6.7).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mounts: BTreeMap<String, Mount>,
    /// Logging defaults (section 11.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Logging>,
    /// Requests a worker serves before it is replaced (section 7.7),
    /// with jitter; off when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    /// How long a worker lives before it is replaced (section 7.7), for
    /// example 1h, with jitter; off when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime: Option<String>,
    /// The timeouts the host enforces on its workers (section 7.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<Timeouts>,
}

/// The `[timeouts]` table of a release or a `[tool.muru]` table (section
/// 7.6): the timeouts the host enforces on its workers. The edge's own
/// timeouts are the daemon's `[limits]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    /// How long a request may run in a worker before the worker is
    /// replaced, for example 60s; off when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_timeout: Option<String>,
    /// How long a worker being replaced is given to finish its other
    /// requests before it is told to stop (default 10s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_drain_grace: Option<String>,
    /// How long a worker told to stop is given before it is killed
    /// (default 5s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_timeout: Option<String>,
}

/// The `[logging.python]` table (section 11.4): what loggers without
/// handlers of their own do, set before the application is imported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonLogging {
    /// The root logger's level; Python's WARNING when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<LogLevel>,
    /// The format of the platform handler: standard, json, or a logging
    /// format string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Whether warnings.warn goes through the py.warnings logger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_warnings: Option<bool>,
    /// Levels for named loggers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub loggers: BTreeMap<String, LogLevel>,
}

/// The `[logging]` table: the Python logging defaults, with the sinks
/// and rotation of sections 11.2 and 11.3 to follow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logging {
    /// Python logging defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<PythonLogging>,
}

/// The `[dashboard]` table (section 10.7): where the daemon serves its
/// read-only dashboard and JSON gateway.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dashboard {
    /// The loopback address the dashboard is served on, such as
    /// 127.0.0.1:8090; not served when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
}

/// An `[[events.sink]]` entry (section 11.5): a webhook that receives
/// events as signed CloudEvents requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventSink {
    /// The sink's name.
    pub name: String,
    /// The kind of sink; only webhook for now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SinkKind>,
    /// Where deliveries go, an http:// or https:// URL.
    pub url: String,
    /// The key deliveries are signed with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// A file holding the key, instead of `secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_file: Option<String>,
    /// Event types delivered, as patterns such as `release.*`; every
    /// type when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
    /// Levels delivered (info, notice, warning, error); every level when
    /// empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<String>,
}

/// The kind of an event sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SinkKind {
    /// Signed HTTP deliveries.
    Webhook,
}

words!(SinkKind { Webhook => "webhook" });

/// The `[events]` table: the sinks events are pushed to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Events {
    /// The sinks, each an `[[events.sink]]` entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sink: Vec<EventSink>,
}

/// The `[limits]` table (appendix C): sizes and durations as the
/// configuration writes them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Largest request body, for example 10MB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_size: Option<String>,
    /// Largest request head, for example 64KB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_header_bytes: Option<String>,
    /// Most headers in a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_headers: Option<usize>,
    /// Time allowed for a complete request head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_read_timeout: Option<String>,
    /// Time a request may wait for a worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_timeout: Option<String>,
    /// Time a request may take to produce its response head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_deadline: Option<String>,
    /// Time a client may pause while sending a body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_idle_timeout: Option<String>,
    /// Time a worker may pause mid-response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_idle_timeout: Option<String>,
}

/// A listener, one of the `[[listener]]` entries (appendix C), in the
/// daemon's own file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    /// The address, as IP:PORT.
    pub address: String,
    /// Whether TLS is terminated on it, with the certificates of the
    /// virtual hosts it serves, chosen by SNI.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls: bool,
}

/// How forwarding headers are left after they have been applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardedHeaders {
    /// The configured headers stay for the application, other forwarding
    /// headers are removed.
    Keep,
    /// Every forwarding header is removed; the application sees only the
    /// resolved values.
    Consume,
}

words!(ForwardedHeaders { Keep => "keep", Consume => "consume" });

/// The `[proxy]` table (section 4.7): which peers are trusted to speak
/// for the client, and through which headers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proxy {
    /// Addresses, CIDR ranges, or the words loopback, private and unix.
    /// Nothing is trusted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted: Vec<String>,
    /// The header types applied from trusted peers: forwarded,
    /// x-forwarded-for, x-forwarded-proto, x-forwarded-host,
    /// x-forwarded-port, x-forwarded-prefix and x-real-ip. The first four
    /// when omitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    /// Walk a chain of addresses from the right past trusted proxies to
    /// the first untrusted one, rather than take the rightmost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
    /// What becomes of the forwarding headers once applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_headers: Option<ForwardedHeaders>,
}

/// The header types applied when `[proxy]` names none.
pub const DEFAULT_PROXY_HEADERS: [&str; 4] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
];

/// Every header type `[proxy]` accepts.
pub const PROXY_HEADERS: [&str; 7] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-forwarded-prefix",
    "x-real-ip",
];

/// A virtual host, the `[host.NAME]` table (section 4.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    /// The names it answers to: exact names, or `*.example.com` for one
    /// label. Compared case-insensitively, without a port or trailing dot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// The listener addresses it answers on; every listener when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listeners: Vec<String>,
    /// Its certificate and key, for TLS listeners.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<HostTls>,
    /// The name the host is known by, for `use_canonical_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    /// Redirect plain HTTP requests to HTTPS.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub https_redirect: bool,
    /// Give the application the canonical name as SERVER_NAME rather than
    /// the name matched.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub use_canonical_name: bool,
    /// Answer requests whose host matches no virtual host, which are
    /// otherwise refused with 421.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
}

/// A virtual host's certificate chain and private key, as PEM files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostTls {
    /// The certificate chain, leaf first. A relative path is taken from
    /// the file's directory.
    pub certificate: String,
    /// The private key.
    pub key: String,
}

/// A route, one of the `[[route]]` entries (section 4.5): a path prefix
/// of a virtual host to an application.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// The virtual host.
    pub host: String,
    /// The path prefix, `/` for everything. The longest prefix wins.
    #[serde(default = "root_path")]
    pub path: String,
    /// The application whose serving release takes the requests.
    pub app: String,
}

fn root_path() -> String {
    "/".to_owned()
}

/// An application's table and its releases.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    /// Releases by identifier.
    #[serde(default)]
    pub release: BTreeMap<String, Release>,
}

/// The `[metrics]` table (section 11.1): where the daemon serves the
/// Prometheus exposition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metrics {
    /// The address `/metrics` is served on, an internal one such as
    /// 127.0.0.1:9464; not served when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
}

/// A declarative configuration file as `muructl apply` reads it and
/// `muructl config export` writes it (section 10.4): listeners and limits,
/// which belong to the daemon's own file, and applications with their
/// releases.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// Listeners.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listener: Vec<Listener>,
    /// Limits in force everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
    /// Trusted proxies and the headers taken from them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<Proxy>,
    /// The metrics listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    /// Event sinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Events>,
    /// The dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<Dashboard>,
    /// Virtual hosts by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host: BTreeMap<String, Host>,
    /// Routes, in the order written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route: Vec<Route>,
    /// Applications by name.
    #[serde(default)]
    pub app: BTreeMap<String, App>,
}

impl Document {
    /// The document as TOML.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Parse a document, with unknown keys and wrong types reported the
    /// way toml reports them, with their line.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|error| error.to_string())
    }

    /// Fold another document into this one: its listeners and routes are
    /// appended, its limits replace these when given, and its hosts,
    /// applications and releases are added. A host or a release declared
    /// in both is an error.
    pub fn merge(&mut self, other: Document) -> Result<(), String> {
        self.listener.extend(other.listener);
        self.route.extend(other.route);

        if other.limits.is_some() {
            self.limits = other.limits;
        }

        if other.metrics.is_some() {
            self.metrics = other.metrics;
        }

        if other.events.is_some() {
            self.events = other.events;
        }

        if other.dashboard.is_some() {
            self.dashboard = other.dashboard;
        }

        if other.proxy.is_some() {
            self.proxy = other.proxy;
        }

        for (name, host) in other.host {
            if self.host.contains_key(&name) {
                return Err(format!("host {name} is declared twice"));
            }

            self.host.insert(name, host);
        }

        for (name, app) in other.app {
            let entry = self.app.entry(name.clone()).or_default();

            for (id, release) in app.release {
                if entry.release.contains_key(&id) {
                    return Err(format!("release {name}@{id} is declared twice"));
                }

                entry.release.insert(id, release);
            }
        }

        Ok(())
    }
}

/// The `[tool.muru]` table: defaults an application ships with (section
/// 6.8). Every field is optional; what a command line or a release
/// definition sets takes precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolMuru {
    /// The application as module:callable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    /// The interface the entrypoint speaks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<Interface>,
    /// Whether the entrypoint is a factory returning the application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory: Option<bool>,
    /// When a factory is called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory_phase: Option<FactoryPhase>,
    /// Worker processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<usize>,
    /// Application threads per worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<usize>,
    /// Threads for synchronous code called from an ASGI application
    /// (section 6.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_threads: Option<usize>,
    /// Requests an ASGI worker's event loop takes at once (section 6.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// The preload mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preload: Option<Preload>,
    /// The module providing the lifecycle hooks (section 6.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<String>,
    /// The Python requirement, for example "3.13" or ">=3.12".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// Worker groups by name (section 7.1).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Group>,
    /// Mounts by name (section 6.7).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mounts: BTreeMap<String, Mount>,
    /// Logging defaults (section 11.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Logging>,
    /// Requests a worker serves before it is replaced (section 7.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    /// How long a worker lives before it is replaced (section 7.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime: Option<String>,
    /// The timeouts the host enforces on its workers (section 7.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<Timeouts>,
}

/// Why a project's configuration could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// The file exists but could not be read.
    #[error("reading {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The file is not valid TOML, or the table has a wrong or unknown key.
    #[error("{path}: {message}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong, as toml reports it.
        message: String,
    },
}

/// The names of a project's configuration files, in the order they are tried.
pub const PROJECT_FILES: [&str; 2] = ["muru.toml", "pyproject.toml"];

/// Read a project's `[tool.muru]` table from `dir`: muru.toml (the keys at
/// the top level) is tried first, then pyproject.toml. Returns the file
/// read and the table, or None when the project has neither file or a
/// pyproject.toml without the table.
pub fn load_project(dir: &Path) -> Result<Option<(PathBuf, ToolMuru)>, ProjectError> {
    for name in PROJECT_FILES {
        let path = dir.join(name);

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(ProjectError::Read { path, source }),
        };

        let table = if name == "muru.toml" {
            Some(
                parse_muru_toml(&text).map_err(|message| ProjectError::Invalid {
                    path: path.clone(),
                    message,
                })?,
            )
        } else {
            parse_pyproject(&text).map_err(|message| ProjectError::Invalid {
                path: path.clone(),
                message,
            })?
        };

        if let Some(table) = table {
            return Ok(Some((path, table)));
        }
    }

    Ok(None)
}

/// Parse a muru.toml: the `[tool.muru]` keys at the top level.
pub fn parse_muru_toml(text: &str) -> Result<ToolMuru, String> {
    toml::from_str(text).map_err(|error| error.to_string())
}

/// The `[tool.muru]` table of a pyproject.toml, if it has one.
pub fn parse_pyproject(text: &str) -> Result<Option<ToolMuru>, String> {
    #[derive(Deserialize)]
    struct Pyproject {
        #[serde(default)]
        tool: Tool,
    }
    #[derive(Deserialize, Default)]
    struct Tool {
        #[serde(default)]
        muru: Option<ToolMuru>,
    }
    let project: Pyproject = toml::from_str(text).map_err(|error| error.to_string())?;
    Ok(project.tool.muru)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_round_trip() {
        for word in Preload::ALL {
            assert_eq!(word.parse::<Preload>().unwrap().as_str(), *word);
        }

        assert_eq!("asgi".parse::<Interface>(), Ok(Interface::Asgi));
        let error = "maybe".parse::<Preload>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "\"maybe\" is not a Preload: expected one of auto, true, false"
        );
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(parse_size("10MB"), Ok(10 * 1024 * 1024));
        assert_eq!(parse_size("512kb"), Ok(512 * 1024));
        assert_eq!(parse_size("1G"), Ok(1024 * 1024 * 1024));
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size(" 2 MB "), Ok(2 * 1024 * 1024));
        assert!(parse_size("ten").is_err());
        assert!(parse_size("99999999999999999999GB").is_err());
    }

    #[test]
    fn sizes_and_durations_are_written_in_the_largest_exact_unit() {
        assert_eq!(format_size(100 * 1024 * 1024), "100MB");
        assert_eq!(format_size(64 * 1024), "64KB");
        assert_eq!(format_size(1500), "1500");
        assert_eq!(format_duration(Duration::from_secs(300)), "300s");
        assert_eq!(format_duration(Duration::from_millis(250)), "250ms");
    }

    #[test]
    fn a_document_round_trips_through_toml() {
        let mut document = Document {
            listener: vec![Listener {
                address: "127.0.0.1:8080".to_owned(),
                tls: false,
            }],
            limits: Some(Limits {
                max_body_size: Some("100MB".to_owned()),
                ..Limits::default()
            }),
            ..Document::default()
        };
        document.app.insert(
            "shop".to_owned(),
            App {
                release: BTreeMap::from([(
                    "r1".to_owned(),
                    Release {
                        source: Some("/srv/shop".to_owned()),
                        entrypoint: "shop.wsgi:application".to_owned(),
                        processes: Some(2),
                        preload: Some(Preload::Auto),
                        ..Release::default()
                    },
                )]),
            },
        );
        document.host.insert(
            "shop".to_owned(),
            Host {
                names: vec![
                    "shop.example.com".to_owned(),
                    "*.shop.example.com".to_owned(),
                ],
                tls: Some(HostTls {
                    certificate: "certs/shop.pem".to_owned(),
                    key: "certs/shop-key.pem".to_owned(),
                }),
                https_redirect: true,
                ..Host::default()
            },
        );
        document.route.push(Route {
            host: "shop".to_owned(),
            path: "/".to_owned(),
            app: "shop".to_owned(),
        });
        let text = document.to_toml().unwrap();
        assert!(text.contains("[app.shop.release.r1]"), "{text}");
        assert!(text.contains("[host.shop]"), "{text}");
        assert!(text.contains("[[route]]"), "{text}");
        assert!(
            text.contains("entrypoint = \"shop.wsgi:application\""),
            "{text}"
        );
        assert!(text.contains("[[listener]]"), "{text}");
        let back: Document = toml::from_str(&text).unwrap();
        assert_eq!(back, document);
    }

    #[test]
    fn a_document_rejects_unknown_keys_and_merges_releases() {
        let error = Document::parse("[app.shop.release.r1]\nentry_point = 'a:b'\n").unwrap_err();
        assert!(error.contains("entry_point"), "{error}");

        let mut first = Document::parse("[app.shop.release.r1]\nentrypoint = 'a:b'\n").unwrap();
        let second = Document::parse("[app.shop.release.r2]\nentrypoint = 'a:c'\n").unwrap();
        first.merge(second).unwrap();
        assert_eq!(first.app["shop"].release.len(), 2);

        let again = Document::parse("[app.shop.release.r2]\nentrypoint = 'a:c'\n").unwrap();
        let error = first.merge(again).unwrap_err();
        assert_eq!(error, "release shop@r2 is declared twice");

        let routed = Document::parse("[[route]]\nhost = 'shop'\napp = 'shop'\n").unwrap();
        assert_eq!(routed.route[0].path, "/", "the path defaults to the root");
        first.merge(routed.clone()).unwrap();
        first.merge(routed).unwrap();
        assert_eq!(first.route.len(), 2, "routes accumulate");
    }

    #[test]
    fn pyproject_table_is_optional_and_strict() {
        assert_eq!(parse_pyproject("[project]\nname = 'x'\n").unwrap(), None);
        let table = parse_pyproject(
            "[tool.muru]\nentrypoint = 'shop.wsgi:application'\nprocesses = 2\nfactory = true\nfactory_phase = 'host'\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(table.entrypoint.as_deref(), Some("shop.wsgi:application"));
        assert_eq!(table.processes, Some(2));
        assert_eq!(table.factory, Some(true));
        assert_eq!(table.factory_phase, Some(FactoryPhase::Host));
        let error = parse_pyproject("[tool.muru]\nworkers = 2\n").unwrap_err();
        assert!(error.contains("workers"), "{error}");
        let error = parse_pyproject("[tool.muru]\npreload = 'maybe'\n").unwrap_err();
        assert!(error.contains("maybe"), "{error}");
    }

    #[test]
    fn logging_defaults_are_a_nested_table() {
        let table = parse_pyproject(
            "[tool.muru]\nentrypoint = 'a:b'\n[tool.muru.logging.python]\nlevel = 'INFO'\nformat = 'json'\ncapture_warnings = true\nloggers = { noisy = 'ERROR' }\n",
        )
        .unwrap()
        .unwrap();
        let python = table.logging.unwrap().python.unwrap();
        assert_eq!(python.level, Some(LogLevel::Info));
        assert_eq!(python.format.as_deref(), Some("json"));
        assert_eq!(python.capture_warnings, Some(true));
        assert_eq!(python.loggers.get("noisy"), Some(&LogLevel::Error));
        let error = parse_pyproject("[tool.muru.logging.python]\nlevel = 'loud'\n").unwrap_err();
        assert!(error.contains("loud"), "{error}");
    }

    #[test]
    fn event_sinks_are_an_array_of_tables() {
        let document = Document::parse(
            "[[events.sink]]\nname = 'ops'\nurl = 'https://example.com/hook'\nsecret = 's'\ntypes = ['release.*']\nlevels = ['warning', 'error']\n",
        )
        .unwrap();
        let sinks = document.events.unwrap().sink;
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "ops");
        assert_eq!(sinks[0].types, vec!["release.*".to_owned()]);
        assert_eq!(sinks[0].levels.len(), 2);
        let error = Document::parse("[[events.sink]]\nname = 'ops'\nurl = 'x'\nkind = 'pigeon'\n")
            .unwrap_err();
        assert!(error.contains("pigeon"), "{error}");
    }

    #[test]
    fn muru_toml_holds_the_keys_at_the_top() {
        let table = parse_muru_toml("entrypoint = 'app:app'\nthreads = 8\n").unwrap();
        assert_eq!(table.entrypoint.as_deref(), Some("app:app"));
        assert_eq!(table.threads, Some(8));
    }

    #[test]
    fn project_files_are_tried_in_order() {
        let dir = std::env::temp_dir().join(format!("muru-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_project(&dir).unwrap(), None);
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.muru]\nentrypoint = 'a:b'\n",
        )
        .unwrap();
        let (path, table) = load_project(&dir).unwrap().unwrap();
        assert!(path.ends_with("pyproject.toml"));
        assert_eq!(table.entrypoint.as_deref(), Some("a:b"));
        std::fs::write(dir.join("muru.toml"), "entrypoint = 'c:d'\n").unwrap();
        let (path, table) = load_project(&dir).unwrap().unwrap();
        assert!(path.ends_with("muru.toml"));
        assert_eq!(table.entrypoint.as_deref(), Some("c:d"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
