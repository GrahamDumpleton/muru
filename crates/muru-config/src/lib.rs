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

pub mod cron;

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
    /// gRPC services: the entrypoint is a registration function given a
    /// server to add grpcio servicers to (section 6.13).
    Grpc,
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

/// How a worker's memory is measured (section 7.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Measure {
    /// Unique set size: pages no other process shares, the default,
    /// since preloaded pages are shared copy-on-write.
    Uss,
    /// Proportional set size: shared pages counted in proportion.
    Pss,
    /// Resident set size: every resident page.
    Rss,
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

words!(Interface { Auto => "auto", Wsgi => "wsgi", Asgi => "asgi", Grpc => "grpc" });
words!(Preload { Auto => "auto", True => "true", False => "false" });
words!(Measure { Uss => "uss", Pss => "pss", Rss => "rss" });
words!(FactoryPhase { Worker => "worker", Host => "host" });

/// What a release runs (section 9.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseKind {
    /// Muru's own workers, from an entrypoint.
    Workers,
    /// An HTTP server Muru starts and supervises, from a command.
    Http,
}

/// The server a managed HTTP release runs (section 9.9), which fixes
/// how it binds and how it is stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerProfile {
    /// gunicorn: `--bind unix:PATH` or `--bind HOST:PORT`, stopped by TERM.
    Gunicorn,
    /// uvicorn: `--uds PATH` or `--host HOST --port PORT`, stopped by TERM.
    Uvicorn,
    /// Granian: `--uds PATH` or `--host HOST --port PORT`, stopped by TERM.
    Granian,
    /// mod_wsgi-express: `--host HOST --port PORT`, stopped by TERM.
    #[serde(rename = "mod_wsgi-express")]
    ModWsgiExpress,
    /// Any server: the command binds through `{bind}` and `stop_signal`
    /// says how it is stopped.
    Generic,
}

/// How a managed HTTP release's instances bind (section 9.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerBind {
    /// A Unix socket per instance under the release's run directory.
    Unix,
    /// A free loopback port per instance.
    Port,
}

words!(ReleaseKind { Workers => "workers", Http => "http" });
words!(ServerProfile {
    Gunicorn => "gunicorn",
    Uvicorn => "uvicorn",
    Granian => "granian",
    ModWsgiExpress => "mod_wsgi-express",
    Generic => "generic",
});
words!(ServerBind { Unix => "unix", Port => "port" });

/// The signals a `stop_signal` may name.
pub const STOP_SIGNALS: [&str; 7] = ["TERM", "INT", "QUIT", "HUP", "USR1", "USR2", "WINCH"];

/// The `readiness` table of a managed HTTP release (section 9.9): how
/// an instance is known to be ready.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    /// The path probed with a GET until it answers 2xx or 3xx; without
    /// it, any HTTP response makes the instance ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// How long the instances get to become ready (default 60s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

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

/// Write a size for a reader, rounded to the largest unit that keeps a
/// digit: 1.5GB, 800MB, 12KB.
pub fn format_size_approx(bytes: u64) -> String {
    let mut value = bytes as f64;

    for unit in ["B", "KB", "MB", "GB"] {
        if value < 1024.0 || unit == "GB" {
            return if unit == "B" || value >= 10.0 {
                format!("{value:.0}{unit}")
            } else {
                format!("{value:.1}{unit}")
            };
        }

        value /= 1024.0;
    }

    bytes.to_string()
}

/// Parse a duration such as 250ms, 1.5s, 2m, 1h or 2 (seconds), as the
/// configuration and the command line write them.
pub fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
    let text = text.trim();

    let (number, scale) = if let Some(number) = text.strip_suffix("ms") {
        (number, 0.001)
    } else if let Some(number) = text.strip_suffix('y') {
        (number, 365.0 * 86400.0)
    } else if let Some(number) = text.strip_suffix('d') {
        (number, 86400.0)
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
    /// The path prefix; the longest wins, on segment boundaries. A gRPC
    /// mount has none: its calls are matched by service name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// The application as module:callable.
    pub entrypoint: String,
    /// The interface, when declared rather than detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<Interface>,
    /// Reachable only through internal redirects (section 4.10): a
    /// client's request for its path is answered 404 (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal: Option<bool>,
    /// Other applications whose internal redirects may reach the mount
    /// when it is internal; the release's own always may.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept_redirects_from: Vec<String>,
    /// Treat an `X-Accel-Redirect` response header from the mount as an
    /// internal redirect (default the release's setting, else false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accel_redirect: Option<bool>,
    /// Treat a 200 response with a `Location` starting with `/` and no
    /// body as an internal redirect, the CGI convention (default the
    /// release's setting, else false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgi_local_redirect: Option<bool>,
    /// Directories a path in an `X-Muru-Sendfile` response header must
    /// lie in for the edge to serve the file (section 4.9); the
    /// release's when absent, and an empty list refuses every hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sendfile_roots: Option<Vec<String>>,
    /// A second response header honoured as `X-Muru-Sendfile` is, such
    /// as `X-Sendfile` (default the release's setting, else none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sendfile_header: Option<String>,
}

/// A worker group, the `[app.NAME.release.ID.groups.NAME]` or
/// `[tool.muru.groups.NAME]` table (section 7.1): the paths it serves
/// and its own sizes. Paths not claimed by a group go to the default
/// group, which is the release's own settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// Path prefixes, with an optional trailing `*`; the most specific
    /// match across all groups wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// The client classes (section 4.14) the group takes: their requests
    /// for its paths, or for every path without `paths`, ahead of the
    /// groups that name no class.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<ClientMatch>,
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
    /// How the group is scaled (sections 7.2 and 7.4); static when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaling: Option<Scaling>,
}

/// A release definition, the `[app.NAME.release.ID]` table (appendix C):
/// the keys a deploy takes. `muructl run --print-config` writes one so a
/// laptop setup can become a declared release, and `muructl apply` reads
/// them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// What the release runs (section 9.9): `workers` from an entrypoint,
    /// the default, or `http`, an HTTP server started from `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ReleaseKind>,
    /// The server an `http` release runs: gunicorn, uvicorn, granian,
    /// mod_wsgi-express or generic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ServerProfile>,
    /// The command an `http` release runs, with `{bind}` where the
    /// server takes its address; without `{bind}` the profile adds the
    /// server's own flags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// The directory an `http` release's command runs in; `source` when
    /// absent, and one of the two is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// How an `http` release's instances bind: `unix` (the default) or
    /// `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<ServerBind>,
    /// How many instances of the command an `http` release runs (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<u32>,
    /// How an `http` release's instances are known to be ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<Readiness>,
    /// The signal that stops an `http` release's instance gracefully:
    /// TERM, INT, QUIT, HUP, USR1, USR2 or WINCH. Required by the generic
    /// profile; the other profiles know their server's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
    /// The source directory. A relative path is taken from the file's
    /// directory. Required by `muructl apply` for a `workers` release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The application as module:callable, else from the source's
    /// `[tool.muru]` table.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub entrypoint: String,
    /// The interface, when declared rather than detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<Interface>,
    /// Serve gRPC server reflection for the services the release's gRPC
    /// mounts register (section 6.13, default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflection: Option<bool>,
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
    /// How the default group is scaled (sections 7.2 and 7.4); static
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaling: Option<Scaling>,
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
    /// The memory policy for the workers (section 7.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Memory>,
    /// Environment variables for the workers (section 10.8): a value, or
    /// a secret reference resolved when the release starts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, EnvValue>,
    /// Treat an `X-Accel-Redirect` response header as an internal
    /// redirect (section 4.10), for the default mount and any mount that
    /// does not say otherwise (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accel_redirect: Option<bool>,
    /// Treat a 200 response with a `Location` starting with `/` and no
    /// body as an internal redirect, the CGI convention (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgi_local_redirect: Option<bool>,
    /// Directories a path in an `X-Muru-Sendfile` response header must
    /// lie in for the edge to serve the file (section 4.9), for the
    /// default mount and any mount that does not say otherwise; with
    /// none every hint is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sendfile_roots: Vec<String>,
    /// A second response header honoured as `X-Muru-Sendfile` is, such
    /// as `X-Sendfile` for the frameworks that send that one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sendfile_header: Option<String>,
    /// Custom error pages (section 4.12), by status or status:reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub error_pages: BTreeMap<String, ErrorPage>,
    /// Replace the application's own error responses with error pages
    /// (section 4.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_override: Option<ErrorOverride>,
    /// What maintenance mode lets through (section 4.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<Maintenance>,
}

/// How a worker group is scaled (section 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScalingMode {
    /// A fixed worker count, the default.
    Static,
    /// Keep between min_spare and max_spare idle workers, Apache-style.
    Spare,
    /// Track a slot utilization and a queue wait target.
    Target,
}

impl ScalingMode {
    /// The mode as the configuration spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Spare => "spare",
            Self::Target => "target",
        }
    }
}

/// The `scaling` table of a release, a worker group or a `[tool.muru]`
/// table (sections 7.2 and 7.4): the mode, the bounds, and the windows
/// and budgets that keep the autoscaler from churning. The release's
/// own table is the default group's.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scaling {
    /// static (the default), spare or target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ScalingMode>,
    /// The fewest workers (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_workers: Option<usize>,
    /// The most workers; required by the spare and target modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_workers: Option<usize>,
    /// Spare mode: scale out when fewer idle workers than this remain
    /// (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_spare: Option<usize>,
    /// Spare mode: scale in while more idle workers than this remain
    /// (default the larger of 3 and a quarter of max_workers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spare: Option<usize>,
    /// Target mode: the share of slots to keep busy, between 0 and 1
    /// (default 0.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_utilization: Option<f64>,
    /// Target mode: scale out while the p95 queue wait is over this,
    /// for example 100ms; off when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_wait_p95: Option<String>,
    /// How long a scale-out condition must hold (default 5s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_out_window: Option<String>,
    /// How long every scale-in condition must hold (default 10m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_in_window: Option<String>,
    /// No scale-in this long after a scale-out (default 5m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_after_growth: Option<String>,
    /// At most one worker reaped per this interval (default 60s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reap_interval: Option<String>,
    /// A group wholly idle this long scales in without waiting for the
    /// scale-in window (default 10m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_reap_after: Option<String>,
    /// No worker younger than this is reaped (default the larger of 15m
    /// and 30 times the measured time to ready).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_lifetime: Option<String>,
    /// The most workers one scale-out adds (default 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_step_up: Option<usize>,
    /// Reaps allowed per hour per release before scale-in is suspended
    /// (default 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub churn_budget: Option<usize>,
    /// Decide and report without acting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisory: Option<bool>,
}

/// The `[memory]` table of a release or a `[tool.muru]` table (section
/// 7.7): the limits a worker's memory is held to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    /// What is measured: uss (the default), pss or rss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<Measure>,
    /// Over this for soft_for, the worker reclaims and then recycles,
    /// for example 800MB; off when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_limit: Option<String>,
    /// How long the worker may stay over the soft limit (default 2m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_for: Option<String>,
    /// Whether to collect garbage and trim the heap before recycling
    /// over the soft limit (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclaim_first: Option<bool>,
    /// Over this the worker leaves routing at once and is replaced,
    /// for example 1200MB; off when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_limit: Option<String>,
    /// How often memory is measured (default 5s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_interval: Option<String>,
}

/// A value in an `[env]` table (section 10.8): the text itself, or a
/// reference to a secret in a store, delivered as the variable or, with
/// `as = 'file'`, as a file whose path is in `NAME_FILE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EnvValue {
    /// The value as written.
    Plain(String),
    /// A secret reference.
    Secret(SecretValue),
}

/// A secret reference in an `[env]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretValue {
    /// The store and key.
    pub secret: SecretRef,
    /// `file` to deliver the value as a file instead of the variable.
    #[serde(rename = "as", default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<String>,
}

/// Which secret: a store declared in the daemon's file, and a key in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    /// The store's name.
    pub store: String,
    /// The key within the store.
    pub key: String,
}

/// The `[logs]` table of the daemon's file (section 11.3): how the
/// daemon rotates the log files it and its processes write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logs {
    /// `built-in` (the default) or `external`, for logrotate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate: Option<String>,
    /// Rotate a file that reaches this size, for example 100MB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// Rotate by the clock: `off`, `hourly`, `daily` (the default) or
    /// `weekly`, at the period's start in UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    /// Archives kept per log (default 14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<usize>,
    /// How archives are compressed: `zstd` (the default), `gzip` or `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compress: Option<String>,
}

/// The `[daemon]` table of the daemon's file: how the daemon itself runs
/// (ADR 0026).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Daemon {
    /// The service user: whom a daemon that runs as root starts the edge,
    /// and the applications that name no user of their own, as (default
    /// `muru`). Never root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Allow `muructl debug exec`, which runs code in a worker (section
    /// 11.7); off unless set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_exec: Option<bool>,
}

/// The `[secrets]` table of the daemon's file (section 10.8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    /// Stores by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub store: BTreeMap<String, SecretStore>,
}

/// One `[secrets.store.NAME]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretStore {
    /// `file`, `env` or `exec`.
    #[serde(default)]
    pub kind: String,
    /// For a file store, the directory; the daemon's secrets directory
    /// under the store's name otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// For an exec store, the command and arguments, with `{key}` where
    /// the key goes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// For an exec store, how long the command may take (default 5s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// The `[timeouts]` table of a release or a `[tool.muru]` table (section
/// 7.6): the timeouts the host enforces on its workers. The edge's own
/// timeouts are the daemon's `[limits]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    /// How long a request may run in a worker before it is interrupted
    /// by raising `muru.RequestTimeout` in its thread, for example 30s;
    /// off when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_timeout: Option<String>,
    /// How long a request may run in a worker before the worker is
    /// replaced, for example 60s; off when absent. Must exceed the soft
    /// timeout by at least 5s.
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
    /// How long an ASGI worker's event loop may go without running
    /// before the worker is replaced as for the hard timeout (default
    /// 10s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_stall_timeout: Option<String>,
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
/// dashboard and JSON gateway, and whether it takes actions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dashboard {
    /// The loopback address the dashboard is served on, such as
    /// 127.0.0.1:8090; not served when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// Whether a person signed in with `muructl dashboard login` may act
    /// from the dashboard; read-only when false, the default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub actions: bool,
}

/// The `[overlay_webhook]` table (section 9.10): where the daemon takes
/// signed requests from monitoring and alerting systems to activate and
/// end the overlays that declare the external trigger.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayWebhook {
    /// The address served, such as 0.0.0.0:9466.
    pub listen: String,
    /// The key requests are signed with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// A file holding the key, in place of `secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_file: Option<String>,
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
    /// Open connections per listener (section 4.18).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<usize>,
    /// Open connections per client address; 0 disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections_per_client: Option<usize>,
    /// Idle time before a keep-alive connection is closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive_timeout: Option<String>,
    /// The same for connections from trusted proxies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive_timeout_proxy: Option<String>,
    /// Requests served on one connection before it is closed; 0 for no
    /// limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests_per_connection: Option<u64>,
    /// HTTP/2 settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http2: Option<Http2Limits>,
    /// What stored uploads may hold at once across every application
    /// (section 4.11); no limit without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spool_quota: Option<String>,
}

/// The `[limits.http2]` table (section 4.18).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Http2Limits {
    /// Concurrent streams per connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_streams: Option<u32>,
}

/// The `[compression]` table (section 4.17): how the edge compresses
/// responses. Every key is optional, with the defaults of appendix C.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compression {
    /// Whether responses are compressed at all (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The encodings offered in preference order: br, zstd and gzip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub encodings: Vec<String>,
    /// The smallest body worth compressing, for example 1KB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_size: Option<String>,
    /// The content types compressed: exact types or a family as text/*.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
    /// How hard to compress: fast, default or best.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
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
    /// Custom error pages (section 4.12), by status or status:reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub error_pages: BTreeMap<String, ErrorPage>,
    /// Replace the application's own error responses with error pages
    /// (section 4.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_override: Option<ErrorOverride>,
    /// The `[rate_limit.NAME]` tables applied to every request for the
    /// host (section 4.15), before a route's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_limits: Vec<String>,
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
    /// A name for the route, unique among routes, by which overlays
    /// change it (section 9.10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The virtual host.
    pub host: String,
    /// The path prefix, `/` for everything. The longest prefix wins.
    #[serde(default = "root_path")]
    pub path: String,
    /// The client classes (section 4.14) the route takes; every request
    /// without it. Among routes with the same path, one naming the
    /// request's class comes first.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<ClientMatch>,
    /// The application whose serving release takes the requests; empty
    /// for a static or upstream route.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    /// The HTTP upstream (section 4.13) that takes the requests, instead
    /// of an application.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upstream: String,
    /// The directory served by the edge (section 4.9), instead of an
    /// application.
    #[serde(rename = "static", default, skip_serializing_if = "Option::is_none")]
    pub static_dir: Option<String>,
    /// The files tried, in order, when a directory is requested
    /// (default index.html).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub index: Vec<String>,
    /// What takes a request no file answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Fallback>,
    /// The cache headers files are sent with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheTable>,
    /// Whether a directory asked for without a trailing slash is
    /// redirected to the slash form (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_slash: Option<bool>,
    /// `strict` (the default), which matches names as they are on disk,
    /// or `insensitive`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case: Option<String>,
    /// How requests are shared between the application's releases
    /// (section 9.4); without it every request goes to the serving
    /// release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic: Option<Traffic>,
    /// Reachable only through internal redirects (section 4.10): a
    /// client's request for the path is answered 404 (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal: Option<bool>,
    /// Applications other than the route's own (or a static route's
    /// fallback) whose internal redirects may reach the route. A static
    /// route with no fallback needs it for any redirect to reach it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept_redirects_from: Vec<String>,
    /// Custom error pages (section 4.12), by status or status:reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub error_pages: BTreeMap<String, ErrorPage>,
    /// Replace the application's own error responses with error pages
    /// (section 4.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_override: Option<ErrorOverride>,
    /// An authorisation decision the edge asks an application for before
    /// it serves the route (section 4.16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    /// The largest request body, for example 1GB, replacing the daemon's
    /// `[limits]` value for the route (section 4.6). On an upload route
    /// it bounds the body as received, files included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_size: Option<String>,
    /// Section 4.11: `multipart` or `raw` makes the edge receive and store
    /// uploads before the application sees the request; off without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<String>,
    /// The largest uploaded file, for example 512MB; no limit without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_size: Option<String>,
    /// The most files in one request (100 by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files: Option<u32>,
    /// The digests computed while receiving: sha256 (the default),
    /// sha384, sha512 or sha1; an empty list for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<Vec<String>>,
    /// Ask the application with the request's headers alone first, and
    /// read the body only when it answers 2xx (false by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precheck: Option<bool>,
    /// What the application's stored uploads may hold at once when a
    /// request on the route arrives (10GB by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spool_quota: Option<String>,
    /// The `[rate_limit.NAME]` tables applied to the route's requests
    /// (section 4.15), after the host's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_limits: Vec<String>,
    /// Methods the route refuses (section 4.15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub methods: Option<Methods>,
    /// The site mode passed to the application as muru.mode, for example
    /// read-only (section 4.15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

impl Route {
    /// The client classes the route is for, empty for every request.
    pub fn classes(&self) -> &[String] {
        self.matches
            .as_ref()
            .map_or(&[], |matches| matches.client_class.as_slice())
    }

    /// What identifies the route within its host: the path, followed by
    /// the classes it is for, so routes for different classes may share
    /// a path.
    pub fn key(&self) -> String {
        let mut classes = self.classes().to_vec();

        if classes.is_empty() {
            return self.path.clone();
        }

        classes.sort();
        classes.dedup();
        format!("{} client_class={}", self.path, classes.join(","))
    }
}

/// A route's method restrictions (section 4.15).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Methods {
    /// The methods refused, for example POST, PUT, PATCH and DELETE.
    #[serde(default)]
    pub deny: Vec<String>,
    /// `read_only` (the default) answers 503 read_only;
    /// `method_not_allowed` answers 405 with the methods left in Allow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

/// Check a route's rate limit names, method restrictions and mode.
/// Whether the rate limits exist is for the daemon to check.
pub fn check_route_policies(route: &Route) -> Result<(), String> {
    for name in &route.rate_limits {
        check_policy_name("rate_limits", name)?;
    }

    check_client_match(route.matches.as_ref())?;

    for class in route
        .traffic
        .iter()
        .flat_map(|traffic| traffic.classes.keys())
    {
        check_policy_name("traffic.classes", class)?;
    }

    if let Some(methods) = &route.methods {
        if methods.deny.is_empty() {
            return Err("methods.deny names no method".to_owned());
        }

        for method in &methods.deny {
            if method.is_empty() || !method.bytes().all(|byte| byte.is_ascii_uppercase()) {
                return Err(format!(
                    "methods.deny: {method:?} is not a method such as POST"
                ));
            }
        }

        if let Some(answer) = &methods.answer
            && answer != "read_only"
            && answer != "method_not_allowed"
        {
            return Err(format!(
                "methods.answer must be read_only or method_not_allowed, not {answer:?}"
            ));
        }
    }

    if let Some(mode) = &route.mode
        && (mode.is_empty() || mode.len() > 64 || !mode.bytes().all(|byte| byte.is_ascii_graphic()))
    {
        return Err(format!(
            "mode: {mode:?} must be a short word such as read-only"
        ));
    }

    Ok(())
}

/// Check a route's or worker group's `match`: it names at least one
/// client class, each a valid name. `default` is the class of requests
/// no rule matches, and may be named.
pub fn check_client_match(matches: Option<&ClientMatch>) -> Result<(), String> {
    let Some(matches) = matches else {
        return Ok(());
    };

    if matches.client_class.is_empty() {
        return Err("match.client_class names no class".to_owned());
    }

    for class in &matches.client_class {
        check_policy_name("match.client_class", class)?;
    }

    Ok(())
}

/// A client class or rate limit name: lower case letters, digits, `_`
/// and `-`, as `ai_crawler` or `crawlers`.
fn check_policy_name(key: &str, name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        });

    if valid {
        Ok(())
    } else {
        Err(format!(
            "{key}: {name:?} must be lower case letters, digits, _ and -"
        ))
    }
}

/// A `[[client_class]]` rule (section 4.14). A request gets the class
/// when every input the rule names matches, each input matching when any
/// of its entries does; rules are tried in order and a request that
/// matches none is class `default`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientClass {
    /// The class's name, as logs, metrics, rate limits and applications
    /// see it.
    pub name: String,
    /// User-Agent substrings, compared without regard to case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_agent: Vec<String>,
    /// User-Agent regular expressions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_agent_regex: Vec<String>,
    /// A file of User-Agent substrings, one a line, relative to the
    /// configuration file's directory and read again when it changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent_list: Option<String>,
    /// Client addresses and networks, such as 203.0.113.0/24.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    /// A file of addresses and networks, one a line, as for
    /// `user_agent_list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address_list: Option<String>,
    /// Request headers by name, each value matched by a regular
    /// expression: a CDN's verdict, say.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Cookies by name, each value matched by a regular expression.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cookies: BTreeMap<String, String>,
    /// `dns` confirms the client's address by reverse and forward lookup
    /// against `verify_domains`; until it is confirmed the rule does not
    /// match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
    /// The domains a verified name must be, or end in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify_domains: Vec<String>,
}

/// The class a request matching no rule gets.
pub const DEFAULT_CLIENT_CLASS: &str = "default";

/// An `[overlay.NAME]` table (section 9.10): changes to named routes and
/// to applications, applied over the base configuration while the
/// overlay is active, by its schedule or by hand.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overlay {
    /// Changes to routes, by the route's `name`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub routes: BTreeMap<String, RouteOverlay>,
    /// Changes to applications, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub apps: BTreeMap<String, AppOverlay>,
    /// When the overlay is active on its own; by hand only without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<OverlayTrigger>,
    /// What must hold for the overlay to activate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preconditions: Option<Preconditions>,
    /// Which of two active overlays setting the same field wins: the
    /// higher. Overlays that would conflict must both set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Once its schedule activates it, it stays active past the window
    /// until a person ends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latch: Option<bool>,
    /// Its schedule only raises overlay.would_activate events; it is
    /// activated by hand only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisory: Option<bool>,
    /// What to start ahead of its schedule's windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_up: Option<WarmUp>,
}

/// An overlay's warm-up: for `lead` before each window of its schedule,
/// `start_release` runs, so a switch to it lands on warm workers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmUp {
    /// How long before the window, such as 30m.
    pub lead: String,
    /// The release to start, as app@release, or as release when the
    /// overlay changes routes of one application only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_release: Option<String>,
}

/// What an overlay changes on a route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteOverlay {
    /// The site mode (section 4.15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Methods refused, as the route's `methods.deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
    /// How refused methods are answered, as the route's `methods.answer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// The traffic policy: a pin sends every request to one release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic: Option<TrafficOverlay>,
    /// What serves the route instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<RouteTarget>,
    /// The rate limits, in place of the route's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<Vec<String>>,
    /// Error pages (section 4.12), by status or status:reason, each in
    /// place of the route's page for the same key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_pages: Option<BTreeMap<String, ErrorPage>>,
    /// Routes for client classes (section 4.14) at the route's host and
    /// path, by class: each a copy of the route with these changes, or the
    /// route for that class already there with them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_routes: Option<BTreeMap<String, RouteOverlay>>,
}

/// An overlay's traffic policy for a route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrafficOverlay {
    /// Send every request to this release of the route's application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// Shares by release, in place of the route's split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<BTreeMap<String, u32>>,
}

/// What serves a route under an overlay: one of an application, a
/// static directory or an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteTarget {
    /// An application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// A directory the edge serves (section 4.9).
    #[serde(rename = "static", default, skip_serializing_if = "Option::is_none")]
    pub static_dir: Option<String>,
    /// An HTTP upstream (section 4.13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

/// What an overlay changes on an application: its maintenance mode, and
/// the capacity of section 7.5, for its default worker group or `group`.
/// Capacity does not conflict between overlays: the highest floor and
/// ceiling stand.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppOverlay {
    /// Maintenance mode (section 4.12) while the overlay is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<bool>,
    /// The site mode (section 4.15) on each of the application's routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Methods refused on each of the application's routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
    /// How refused methods are answered, as a route's `methods.answer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// The worker group the capacity applies to; the default group
    /// without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The fewest workers, a floor over the group's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_workers: Option<u32>,
    /// The most workers, a ceiling over the group's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_workers: Option<u32>,
    /// `hold` keeps the group from scaling in; `normal` (the default)
    /// leaves it to the autoscaler's rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_in: Option<String>,
    /// Refuse deploys and rollouts of the application unless overridden
    /// (section 9.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeze_deploys: Option<bool>,
}

impl AppOverlay {
    /// Whether it sets any capacity.
    pub fn sets_capacity(&self) -> bool {
        self.min_workers.is_some() || self.max_workers.is_some() || self.scale_in.is_some()
    }
}

/// An `[[app.NAME.schedule]]` entry (section 7.5): capacity for a window,
/// shorthand for an overlay named `APP.NAME` that sets only the floor,
/// the ceiling, how scale-in behaves and a deploy freeze.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacitySchedule {
    /// The schedule's name, unique within the application.
    pub name: String,
    /// A one-off window's start, as for an overlay's schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// A one-off window's end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    /// A recurring window's start times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// A recurring window's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    /// The time zone of local times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// The worker group; the default group without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_workers: Option<u32>,
    /// The ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_workers: Option<u32>,
    /// How long before each window the floor applies, such as 30m.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_up: Option<String>,
    /// `hold` or `normal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_in: Option<String>,
    /// Refuse deploys and rollouts during the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeze_deploys: Option<bool>,
}

impl CapacitySchedule {
    /// The overlay the schedule stands for, on application `app`.
    pub fn overlay(&self, app: &str) -> Overlay {
        let change = AppOverlay {
            maintenance: None,
            group: self.group.clone(),
            min_workers: self.min_workers,
            max_workers: self.max_workers,
            scale_in: self.scale_in.clone(),
            freeze_deploys: self.freeze_deploys,
            ..AppOverlay::default()
        };
        let schedule = OverlaySchedule {
            start: self.start.clone(),
            end: self.end.clone(),
            cron: self.cron.clone(),
            duration: self.duration.clone(),
            timezone: self.timezone.clone(),
        };
        Overlay {
            apps: BTreeMap::from([(app.to_owned(), change)]),
            trigger: Some(OverlayTrigger {
                schedule: Some(schedule),
                external: None,
            }),
            warm_up: self.warm_up.clone().map(|lead| WarmUp {
                lead,
                start_release: None,
            }),
            ..Overlay::default()
        }
    }

    /// The schedule an overlay named `APP.NAME` stands for, as (APP,
    /// schedule), when it is one.
    pub fn from_overlay(name: &str, overlay: &Overlay) -> Option<(String, Self)> {
        let (app, schedule_name) = name.split_once('.')?;
        let change = overlay.apps.get(app)?;
        let schedule = overlay.trigger.as_ref()?.schedule.as_ref()?;
        let found = Self {
            name: schedule_name.to_owned(),
            start: schedule.start.clone(),
            end: schedule.end.clone(),
            cron: schedule.cron.clone(),
            duration: schedule.duration.clone(),
            timezone: schedule.timezone.clone(),
            group: change.group.clone(),
            min_workers: change.min_workers,
            max_workers: change.max_workers,
            warm_up: overlay.warm_up.as_ref().map(|warm_up| warm_up.lead.clone()),
            scale_in: change.scale_in.clone(),
            freeze_deploys: change.freeze_deploys,
        };
        (found.overlay(app) == *overlay).then(|| (app.to_owned(), found))
    }
}

/// When an overlay is active on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayTrigger {
    /// A window in time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<OverlaySchedule>,
    /// Whether the signed webhook of the `[overlay_webhook]` table may
    /// activate and end it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<bool>,
}

/// A window in time: one-off, from `start` until `end` when it has one,
/// or recurring, from each time `cron` names for `duration`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlaySchedule {
    /// When it starts, as a local date and time such as
    /// 2026-11-02T09:00, or with an offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// When it ends; it stays active until ended or promoted without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    /// When each window starts, as a cron expression such as
    /// '0 2 * * *' (see [`cron::Cron`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// How long each recurring window lasts, such as 1h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    /// The time zone local times are in, such as Australia/Sydney; the
    /// daemon's own zone without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// What must hold for an overlay to activate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preconditions {
    /// A release that must be ready, as app@release, or as release when
    /// the overlay changes routes of one application only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_ready: Option<String>,
}

/// Check an overlay on its own: its name, the changes, the schedule and
/// the preconditions. Whether the routes, applications, releases and
/// limits it names exist is for the daemon to check.
pub fn check_overlay(name: &str, overlay: &Overlay) -> Result<(), String> {
    if overlay.routes.is_empty() && overlay.apps.is_empty() {
        return Err(format!(
            "overlay {name}: changes no route and no application"
        ));
    }

    for (route, change) in &overlay.routes {
        check_route_overlay(change, true)
            .map_err(|error| format!("overlay {name}: routes.{route}: {error}"))?;
    }

    let schedule = overlay
        .trigger
        .as_ref()
        .and_then(|trigger| trigger.schedule.as_ref());

    if let Some(schedule) = schedule {
        check_schedule(schedule).map_err(|error| format!("overlay {name}: schedule: {error}"))?;
    }

    if let Some(warm_up) = &overlay.warm_up {
        if schedule.is_none() {
            return Err(format!(
                "overlay {name}: warm_up needs a schedule to start ahead of"
            ));
        }

        parse_duration(&warm_up.lead)
            .map_err(|error| format!("overlay {name}: warm_up.lead: {error}"))?;
    }

    for (app, change) in &overlay.apps {
        check_app_overlay(change)
            .map_err(|error| format!("overlay {name}: apps.{app}: {error}"))?;
    }

    if (overlay.latch.is_some() || overlay.advisory.is_some()) && schedule.is_none() {
        return Err(format!(
            "overlay {name}: latch and advisory apply to a schedule, and it has none"
        ));
    }

    Ok(())
}

/// Check what an overlay sets on a route, and on the routes for client
/// classes in `client_routes` when `outer`.
fn check_route_overlay(change: &RouteOverlay, outer: bool) -> Result<(), String> {
    let methods = change.deny.as_ref().map(|deny| Methods {
        deny: deny.clone(),
        answer: change.answer.clone(),
    });
    let probe = Route {
        mode: change.mode.clone(),
        methods,
        rate_limits: change.rate_limits.clone().unwrap_or_default(),
        ..Route::default()
    };
    check_route_policies(&probe)?;

    if change.answer.is_some() && change.deny.is_none() {
        return Err("answer needs deny".to_owned());
    }

    if let Some(pages) = &change.error_pages {
        if pages.is_empty() {
            return Err("error_pages names no page".to_owned());
        }

        check_error_pages(pages, false)?;
    }

    if let Some(traffic) = &change.traffic
        && traffic.pin.is_some() == traffic.split.is_some()
    {
        return Err("traffic sets one of pin and split".to_owned());
    }

    if let Some(target) = &change.target {
        let named = usize::from(target.app.is_some())
            + usize::from(target.static_dir.is_some())
            + usize::from(target.upstream.is_some());

        if named != 1 {
            return Err("target names one of app, static and upstream".to_owned());
        }
    }

    let Some(routes) = &change.client_routes else {
        return Ok(());
    };

    if !outer {
        return Err("client_routes is set on a route, not on a client class's".to_owned());
    }

    if routes.is_empty() {
        return Err("client_routes names no class".to_owned());
    }

    for (class, route) in routes {
        check_policy_name("client_routes", class)?;

        if route == &RouteOverlay::default() {
            return Err(format!("client_routes.{class} sets nothing"));
        }

        check_route_overlay(route, false)
            .map_err(|error| format!("client_routes.{class}: {error}"))?;
    }

    Ok(())
}

/// Check what an overlay sets on an application: a floor of at least
/// one, no higher than the ceiling, and a known `scale_in`.
fn check_app_overlay(change: &AppOverlay) -> Result<(), String> {
    if change.maintenance.is_none()
        && !change.sets_capacity()
        && change.freeze_deploys.is_none()
        && change.mode.is_none()
        && change.deny.is_none()
    {
        return Err("sets nothing".to_owned());
    }

    // Section 4.15: the site mode and methods, as a route's.

    let probe = Route {
        mode: change.mode.clone(),
        methods: change.deny.as_ref().map(|deny| Methods {
            deny: deny.clone(),
            answer: change.answer.clone(),
        }),
        ..Route::default()
    };
    check_route_policies(&probe)?;

    if change.answer.is_some() && change.deny.is_none() {
        return Err("answer needs deny".to_owned());
    }

    if change.group.is_some() && !change.sets_capacity() {
        return Err("group names where capacity applies, and none is set".to_owned());
    }

    if change.min_workers == Some(0) {
        return Err("min_workers must be at least 1".to_owned());
    }

    if let (Some(min), Some(max)) = (change.min_workers, change.max_workers)
        && max < min
    {
        return Err(format!("max_workers {max} is below min_workers {min}"));
    }

    if let Some(scale_in) = &change.scale_in
        && !matches!(scale_in.as_str(), "hold" | "normal")
    {
        return Err(format!("scale_in {scale_in:?} is not hold or normal"));
    }

    Ok(())
}

/// Check a schedule: a `start` with an optional `end`, or a `cron` with
/// a `duration`, in a time zone that exists.
pub fn check_schedule(schedule: &OverlaySchedule) -> Result<(), String> {
    let zone = schedule.timezone.as_deref();

    match (&schedule.start, &schedule.cron) {
        (Some(start), None) => {
            if schedule.duration.is_some() {
                return Err("duration goes with cron; a start has an end".to_owned());
            }

            let start = parse_moment(start, zone).map_err(|error| format!("start: {error}"))?;

            if let Some(end) = &schedule.end {
                let end = parse_moment(end, zone).map_err(|error| format!("end: {error}"))?;

                if end <= start {
                    return Err("end must be after its start".to_owned());
                }
            }
        }
        (None, Some(expression)) => {
            if schedule.end.is_some() {
                return Err("end goes with start; cron has a duration".to_owned());
            }

            let Some(duration) = &schedule.duration else {
                return Err("cron needs a duration, such as 1h".to_owned());
            };

            if parse_duration(duration).map_err(|error| format!("duration: {error}"))?
                < Duration::from_secs(60)
            {
                return Err("duration must be at least a minute".to_owned());
            }

            let cron = cron::Cron::parse(expression)?;
            let zone = time_zone(zone)?;

            if cron.next(jiff::Timestamp::now(), &zone).is_none() {
                return Err(format!("cron {expression:?} never occurs"));
            }
        }
        _ => return Err("give one of start and cron".to_owned()),
    }

    Ok(())
}

/// The window of a schedule that `now` falls in, else the next one, as
/// Unix milliseconds: from its start, until its end when it has one. None
/// when no window is to come, or for a schedule that does not parse.
pub fn schedule_window(schedule: &OverlaySchedule, now: i64) -> Option<(i64, Option<i64>)> {
    let zone = schedule.timezone.as_deref();

    if let Some(start) = &schedule.start {
        let start = parse_moment(start, zone).ok()?;

        let end = match &schedule.end {
            Some(end) => Some(parse_moment(end, zone).ok()?),
            None => None,
        };

        return Some((start, end));
    }

    let cron = cron::Cron::parse(schedule.cron.as_deref()?).ok()?;
    let length = parse_duration(schedule.duration.as_deref()?)
        .ok()?
        .as_millis() as i64;
    let zone = time_zone(zone).ok()?;
    let at = jiff::Timestamp::from_millisecond(now).ok()?;

    // The latest start covers now if any does, as each window is as long.

    if let Some(last) = cron.previous(at, &zone) {
        let start = last.as_millisecond();

        if now < start + length {
            return Some((start, Some(start + length)));
        }
    }

    let next = cron.next(at, &zone)?.as_millisecond();
    Some((next, Some(next + length)))
}

/// A time zone by its IANA name, or the system's without one.
pub fn time_zone(name: Option<&str>) -> Result<jiff::tz::TimeZone, String> {
    match name {
        Some(name) => jiff::tz::TimeZone::get(name)
            .map_err(|_| format!("{name:?} is not a time zone such as Australia/Sydney")),
        None => Ok(jiff::tz::TimeZone::system()),
    }
}

/// A schedule as the reports show it.
pub fn describe_schedule(schedule: &OverlaySchedule) -> String {
    let when = match (&schedule.start, &schedule.cron) {
        (Some(start), _) => format!(
            "from {start}{}",
            schedule
                .end
                .as_ref()
                .map(|end| format!(" until {end}"))
                .unwrap_or_default()
        ),
        (None, Some(cron)) => format!(
            "cron '{cron}' for {}",
            schedule.duration.as_deref().unwrap_or_default()
        ),
        (None, None) => String::new(),
    };

    match &schedule.timezone {
        Some(zone) => format!("{when} {zone}"),
        None => when,
    }
}

/// A moment as Unix milliseconds: an RFC 3339 time with an offset, or a
/// local date and time (seconds optional) in `timezone`, the system's
/// zone without one. A local time that a daylight saving change skips or
/// repeats takes the later or earlier instant as Temporal's compatible
/// rule does.
pub fn parse_moment(text: &str, timezone: Option<&str>) -> Result<i64, String> {
    let text = text.trim();

    if let Ok(instant) = text.parse::<jiff::Timestamp>() {
        return Ok(instant.as_millisecond());
    }

    let zone = time_zone(timezone)?;

    let civil: jiff::civil::DateTime = text
        .parse()
        .map_err(|_| format!("{text:?} is not a date and time such as 2026-11-02T09:00"))?;
    let zoned = civil
        .to_zoned(zone)
        .map_err(|error| format!("{text:?}: {error}"))?;
    Ok(zoned.timestamp().as_millisecond())
}

/// Check a `[[client_class]]` rule: a name, at least one input, regular
/// expressions that compile and a verification with its domains. Whether
/// the list files can be read is for the daemon to check.
pub fn check_client_class(class: &ClientClass) -> Result<(), String> {
    check_policy_name("client_class", &class.name)?;

    if class.name == DEFAULT_CLIENT_CLASS {
        return Err("client_class: default is the class of requests no rule matches".to_owned());
    }

    let inputs = !class.user_agent.is_empty()
        || !class.user_agent_regex.is_empty()
        || class.user_agent_list.is_some()
        || !class.addresses.is_empty()
        || class.address_list.is_some()
        || !class.headers.is_empty()
        || !class.cookies.is_empty();

    if !inputs {
        return Err(format!(
            "client_class {}: names nothing to match",
            class.name
        ));
    }

    for pattern in class
        .user_agent_regex
        .iter()
        .chain(class.headers.values())
        .chain(class.cookies.values())
    {
        regex::Regex::new(pattern).map_err(|error| {
            format!(
                "client_class {}: {pattern:?} is not a regular expression: {error}",
                class.name
            )
        })?;
    }

    match class.verify.as_deref() {
        None if !class.verify_domains.is_empty() => {
            return Err(format!(
                "client_class {}: verify_domains needs verify = 'dns'",
                class.name
            ));
        }
        None => {}
        Some("dns") if class.verify_domains.is_empty() => {
            return Err(format!(
                "client_class {}: verify = 'dns' needs verify_domains",
                class.name
            ));
        }
        Some("dns") => {}
        Some(other) => {
            return Err(format!(
                "client_class {}: verify must be dns, not {other:?}",
                class.name
            ));
        }
    }

    Ok(())
}

/// A `[rate_limit.NAME]` table (section 4.15): a token bucket for each
/// key, which requests of the matching classes draw from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Which requests draw from the buckets; all of them without it.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<ClientMatch>,
    /// What a bucket is kept for: `address` (the default),
    /// `client_class`, `header:NAME`, `cookie:NAME`, or a list of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<RateLimitKey>,
    /// How fast a bucket fills, for example 2/s, 100/m or 1000/h.
    pub rate: String,
    /// How many requests a full bucket allows at once (the rate's count
    /// by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
    /// `reject` (the default) answers 429, `delay` holds the request up
    /// to `max_delay` first, `log` only counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// The longest a request is held under `delay` (1s by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delay: Option<String>,
    /// The leading bits of an IPv4 address that make the key (32).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4_prefix: Option<u8>,
    /// The leading bits of an IPv6 address that make the key (64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv6_prefix: Option<u8>,
    /// Send the RateLimit-Policy and RateLimit fields with a 429.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<bool>,
}

/// The requests a rate limit, route or worker group applies to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientMatch {
    /// Client classes (section 4.14).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_class: Vec<String>,
}

/// One key part or several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RateLimitKey {
    /// A single part.
    One(String),
    /// Parts combined.
    Many(Vec<String>),
}

/// A rate limit with its defaults in place, as `check_rate_limit`
/// resolves it.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitSettings {
    /// The classes it applies to; every class when empty.
    pub classes: Vec<String>,
    /// The key's parts.
    pub key: Vec<String>,
    /// Tokens added each second.
    pub per_second: f64,
    /// The bucket's size.
    pub burst: u32,
    /// reject, delay or log.
    pub action: String,
    /// The longest hold under delay.
    pub max_delay: std::time::Duration,
    /// The address prefixes.
    pub ipv4_prefix: u8,
    /// The address prefixes.
    pub ipv6_prefix: u8,
    /// Whether a 429 carries the RateLimit fields.
    pub headers: bool,
}

/// Parse a rate such as 2/s, 100/m or 1000/h into the count and the
/// period's seconds.
pub fn parse_rate(text: &str) -> Result<(u32, u64), String> {
    let (count, period) = text
        .split_once('/')
        .ok_or_else(|| format!("{text:?} is not a rate such as 2/s, 100/m or 1000/h"))?;
    let count: u32 = count
        .trim()
        .parse()
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| format!("{text:?} is not a rate such as 2/s, 100/m or 1000/h"))?;

    let seconds = match period.trim() {
        "s" | "second" => 1,
        "m" | "minute" => 60,
        "h" | "hour" => 3600,
        _ => {
            return Err(format!(
                "{text:?} is not a rate such as 2/s, 100/m or 1000/h"
            ));
        }
    };

    Ok((count, seconds))
}

/// Check a `[rate_limit.NAME]` table and resolve it with its defaults.
pub fn check_rate_limit(name: &str, limit: &RateLimit) -> Result<RateLimitSettings, String> {
    check_policy_name("rate_limit", name)?;
    let (count, seconds) =
        parse_rate(&limit.rate).map_err(|error| format!("rate_limit {name}: rate: {error}"))?;

    let key = match &limit.key {
        None => vec!["address".to_owned()],
        Some(RateLimitKey::One(part)) => vec![part.clone()],
        Some(RateLimitKey::Many(parts)) => parts.clone(),
    };

    if key.is_empty() {
        return Err(format!("rate_limit {name}: key names no part"));
    }

    for part in &key {
        let valid = matches!(part.as_str(), "address" | "client_class")
            || part
                .strip_prefix("header:")
                .or_else(|| part.strip_prefix("cookie:"))
                .is_some_and(|rest| !rest.is_empty());

        if !valid {
            return Err(format!(
                "rate_limit {name}: key {part:?} must be address, client_class, header:NAME or cookie:NAME"
            ));
        }
    }

    let action = limit.action.clone().unwrap_or_else(|| "reject".to_owned());

    if !matches!(action.as_str(), "reject" | "delay" | "log") {
        return Err(format!(
            "rate_limit {name}: action must be reject, delay or log, not {action:?}"
        ));
    }

    let max_delay = match &limit.max_delay {
        Some(_) if action != "delay" => {
            return Err(format!(
                "rate_limit {name}: max_delay needs action = 'delay'"
            ));
        }
        Some(text) => parse_duration(text)
            .map_err(|error| format!("rate_limit {name}: max_delay: {error}"))?,
        None => std::time::Duration::from_secs(1),
    };

    let ipv4_prefix = limit.ipv4_prefix.unwrap_or(32);
    let ipv6_prefix = limit.ipv6_prefix.unwrap_or(64);

    if ipv4_prefix == 0 || ipv4_prefix > 32 || ipv6_prefix == 0 || ipv6_prefix > 128 {
        return Err(format!(
            "rate_limit {name}: ipv4_prefix is 1 to 32 and ipv6_prefix 1 to 128"
        ));
    }

    let classes = limit
        .matches
        .as_ref()
        .map(|matches| matches.client_class.clone())
        .unwrap_or_default();

    for class in &classes {
        check_policy_name("match.client_class", class)?;
    }

    Ok(RateLimitSettings {
        classes,
        key,
        per_second: f64::from(count) / seconds as f64,
        burst: limit.burst.unwrap_or(count).max(1),
        action,
        max_delay,
        ipv4_prefix,
        ipv6_prefix,
        headers: limit.headers.unwrap_or(false),
    })
}

/// The upload settings of a route with their defaults in place (section
/// 4.11), as `check_upload` resolves them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSettings {
    /// `multipart` or `raw`.
    pub mode: String,
    /// Bytes per file; no limit when none.
    pub max_file_size: Option<u64>,
    /// Files per request.
    pub max_files: u32,
    /// Digest algorithms, in the order given.
    pub hash: Vec<String>,
    /// Ask the application first.
    pub precheck: bool,
    /// Bytes the application's stored uploads may hold at once.
    pub spool_quota: u64,
}

/// The digest algorithms an upload route may ask for.
pub const UPLOAD_HASHES: [&str; 4] = ["sha256", "sha384", "sha512", "sha1"];

/// Files per upload request unless the route says otherwise.
pub const DEFAULT_MAX_FILES: u32 = 100;

/// What an application's stored uploads may hold unless the route says
/// otherwise.
pub const DEFAULT_SPOOL_QUOTA: u64 = 10 * 1024 * 1024 * 1024;

/// Check a route's body limit and upload keys, and resolve the upload
/// settings with their defaults: none when the route takes no uploads.
/// The upload keys other than `upload` need it, and `max_files` stays
/// within what one request can pass to a worker.
pub fn check_upload(route: &Route) -> Result<Option<UploadSettings>, String> {
    if let Some(size) = &route.max_body_size {
        parse_size(size).map_err(|error| format!("max_body_size: {error}"))?;
    }

    let Some(mode) = &route.upload else {
        let stray = [
            ("max_file_size", route.max_file_size.is_some()),
            ("max_files", route.max_files.is_some()),
            ("hash", route.hash.is_some()),
            ("precheck", route.precheck.is_some()),
            ("spool_quota", route.spool_quota.is_some()),
        ];

        return match stray.iter().find(|(_, set)| *set) {
            Some((key, _)) => Err(format!("{key} needs upload = 'multipart' or 'raw'")),
            None => Ok(None),
        };
    };

    if mode != "multipart" && mode != "raw" {
        return Err(format!("upload must be multipart or raw, not {mode:?}"));
    }

    let max_file_size = route
        .max_file_size
        .as_deref()
        .map(parse_size)
        .transpose()
        .map_err(|error| format!("max_file_size: {error}"))?;
    let max_files = route.max_files.unwrap_or(DEFAULT_MAX_FILES);

    if max_files == 0 || max_files > MAX_UPLOAD_FILES {
        return Err(format!(
            "max_files must be from 1 to {MAX_UPLOAD_FILES}, not {max_files}"
        ));
    }

    let hash = route
        .hash
        .clone()
        .unwrap_or_else(|| vec!["sha256".to_owned()]);

    for name in &hash {
        if !UPLOAD_HASHES.contains(&name.as_str()) {
            return Err(format!(
                "hash: {name:?} is not one of {}",
                UPLOAD_HASHES.join(", ")
            ));
        }
    }

    let spool_quota = match &route.spool_quota {
        Some(size) => parse_size(size).map_err(|error| format!("spool_quota: {error}"))?,
        None => DEFAULT_SPOOL_QUOTA,
    };

    Ok(Some(UploadSettings {
        mode: mode.clone(),
        max_file_size,
        max_files,
        hash,
        precheck: route.precheck.unwrap_or(false),
        spool_quota,
    }))
}

/// The most files one request may carry to a worker.
pub const MAX_UPLOAD_FILES: u32 = 500;

/// A route's authentication sub-request (section 4.16): the edge sends a
/// bodiless GET to an application path and serves the route only when
/// that answers 2xx.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// The application that decides; empty when an upstream does.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    /// The HTTP upstream (section 4.13) that decides, instead of an
    /// application.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upstream: String,
    /// The path asked, usually of an internal mount or route.
    pub path: String,
    /// How long the handler has, queueing included, for example 2s (the
    /// default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Response headers of an allowing answer added to the request as
    /// trusted headers; a client's own copies are always removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy_headers: Vec<String>,
    /// Remember allowing decisions for a while (off without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<AuthCache>,
}

/// How long and by what an allowing decision is remembered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthCache {
    /// How long a decision stands, for example 30s.
    pub ttl: String,
    /// What a decision is keyed by: `cookie:NAME` and `header:NAME`
    /// entries. A request with none of them present is never served
    /// from the cache.
    pub key: Vec<String>,
}

/// A route's traffic policy (section 9.4): the share of requests each
/// release takes, the key requests are bucketed by, and stickiness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Traffic {
    /// Percent of requests to each release, by id; the application's
    /// serving release takes the rest. A release at 0 is reached only by
    /// preview or by a sticky cookie.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub split: BTreeMap<String, u32>,
    /// What a request is bucketed by: `cookie` (the default, an anonymous
    /// id cookie the edge issues), `header:NAME`, `cookie:NAME`, or
    /// `client` (the client address). A missing header or cookie falls
    /// back to the client address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_key: Option<String>,
    /// Mixed into the hash, so two routes split the same users
    /// differently; the route's host and path by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salt: Option<String>,
    /// Whether a client keeps the release it first got, through a cookie
    /// (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky: Option<bool>,
    /// How long sticky clients keep a release that is leaving the route
    /// alive, for example 10m (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_grace: Option<String>,
    /// The release, by id, or `upstream:NAME`, that takes every request
    /// of a client class (section 4.14), ahead of stickiness and the
    /// split.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub classes: BTreeMap<String, String>,
}

/// A static route's fallback (section 4.9): an application, or a file
/// within the directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fallback {
    /// The application that takes requests no file answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// The HTTP upstream (section 4.13) that takes them instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The file served for paths with no file of their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// A static route's cache headers (section 4.9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheTable {
    /// How long clients may keep a file, for example 1y or 1h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<String>,
    /// Whether files are marked immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub immutable: Option<bool>,
    /// Files that get no-cache instead, by pattern on their path within
    /// the route.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub except: Vec<String>,
}

fn root_path() -> String {
    "/".to_owned()
}

/// A custom error page (section 4.12): one entry of an `error_pages`
/// table, keyed by status (`404`) or status and reason
/// (`"503:maintenance"`). Exactly one of `file`, `template` and `path`
/// is given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorPage {
    /// A file sent as the body, with the content type its name implies.
    /// A release's page may be a path relative to its source directory;
    /// elsewhere the path is absolute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// A template the edge renders, with `{status}`, `{title}`,
    /// `{reason}`, `{description}`, `{request_id}`, `{retry_after}`,
    /// `{host}` and `{path}` filled in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// An application path the edge asks for the page, as an internal
    /// sub-request that sees `REDIRECT_STATUS` and `muru.error_reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The content type of a template (default `text/html;
    /// charset=utf-8`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

/// Replacing an application's own error responses with error pages
/// (section 4.12), the `error_override` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorOverride {
    /// The statuses replaced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statuses: Vec<u16>,
    /// When a listed response is replaced: `all` (the default),
    /// `empty_body`, `not_json` or `html_only`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

/// What maintenance mode lets through (section 4.12), the `maintenance`
/// table of a release.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Maintenance {
    /// Path prefixes served as usual while the application is in
    /// maintenance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempt_paths: Vec<String>,
    /// Client addresses, or ranges in CIDR form, served as usual.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempt_addresses: Vec<String>,
    /// A `name=value` cookie that lets a client through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypass_cookie: Option<String>,
    /// The `Retry-After` the maintenance page carries, for example 5m
    /// (default 60s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<String>,
}

/// Check an `error_pages` table: the keys name a status, with a reason
/// after a colon or not, and each page is exactly one of a file, a
/// template or a path. With `relative_files` a file may be a relative
/// path, which a release resolves against its source.
pub fn check_error_pages(
    pages: &BTreeMap<String, ErrorPage>,
    relative_files: bool,
) -> Result<(), String> {
    for (key, page) in pages {
        let (status, reason) = match key.split_once(':') {
            Some((status, reason)) => (status, Some(reason)),
            None => (key.as_str(), None),
        };

        let status: u16 = status.parse().unwrap_or(0);

        if !(400..=599).contains(&status) {
            return Err(format!(
                "error_pages: {key:?} is not a status from 400 to 599, or STATUS:reason"
            ));
        }

        if reason.is_some_and(|reason| {
            reason.is_empty()
                || !reason
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }) {
            return Err(format!(
                "error_pages: {key:?} does not name a reason such as no_route"
            ));
        }

        let given = [
            page.file.is_some(),
            page.template.is_some(),
            page.path.is_some(),
        ]
        .into_iter()
        .filter(|given| *given)
        .count();

        if given != 1 {
            return Err(format!(
                "error_pages: {key} needs exactly one of file, template and path"
            ));
        }

        if let Some(path) = &page.path
            && !path.starts_with('/')
        {
            return Err(format!("error_pages: {key}: path must start with /"));
        }

        if let Some(file) = &page.file
            && !relative_files
            && !file.starts_with('/')
        {
            return Err(format!(
                "error_pages: {key}: file must be an absolute path here"
            ));
        }

        if page.content_type.is_some() && page.template.is_none() {
            return Err(format!(
                "error_pages: {key}: content_type goes with a template"
            ));
        }
    }

    Ok(())
}

/// Check an `error_override` table: statuses from 400 to 599, and a
/// known `when`.
pub fn check_error_override(table: &ErrorOverride) -> Result<(), String> {
    if table.statuses.is_empty() {
        return Err("error_override: statuses names nothing".to_owned());
    }

    if let Some(status) = table
        .statuses
        .iter()
        .find(|status| !(400..=599).contains(*status))
    {
        return Err(format!(
            "error_override: {status} is not a status from 400 to 599"
        ));
    }

    if let Some(when) = table.when.as_deref()
        && !matches!(when, "all" | "empty_body" | "not_json" | "html_only")
    {
        return Err(format!(
            "error_override: when must be all, empty_body, not_json or html_only, not {when:?}"
        ));
    }

    Ok(())
}

/// Check a route's `auth` table: the application and path, the timeout,
/// the header names and the cache.
pub fn check_auth(table: &Auth) -> Result<(), String> {
    if table.app.is_empty() && table.upstream.is_empty() {
        return Err("auth: app names no application".to_owned());
    }

    if !table.app.is_empty() && !table.upstream.is_empty() {
        return Err("auth: names both an application and an upstream".to_owned());
    }

    if !table.path.starts_with('/') {
        return Err(format!("auth: path {:?} must start with /", table.path));
    }

    if let Some(text) = &table.timeout
        && let Err(error) = parse_duration(text)
    {
        return Err(format!("auth: timeout: {error}"));
    }

    if let Some(name) = table.copy_headers.iter().find(|name| {
        name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return Err(format!("auth: copy_headers: {name:?} is not a header name"));
    }

    if let Some(name) = table
        .copy_headers
        .iter()
        .find(|name| name.to_ascii_lowercase().starts_with("x-muru-"))
    {
        return Err(format!(
            "auth: copy_headers: {name:?} starts with X-Muru-, which the edge keeps for itself"
        ));
    }

    if let Some(cache) = &table.cache {
        if let Err(error) = parse_duration(&cache.ttl) {
            return Err(format!("auth: cache.ttl: {error}"));
        }

        if cache.key.is_empty() {
            return Err("auth: cache.key names nothing".to_owned());
        }

        if let Some(key) = cache.key.iter().find(|key| {
            !matches!(
                key.split_once(':'),
                Some(("cookie" | "header", name)) if !name.is_empty()
            )
        }) {
            return Err(format!(
                "auth: cache.key: {key:?} must be cookie:NAME or header:NAME"
            ));
        }
    }

    Ok(())
}

/// Check a `maintenance` table: paths, addresses, the cookie and the
/// retry period.
pub fn check_maintenance(table: &Maintenance) -> Result<(), String> {
    if let Some(path) = table
        .exempt_paths
        .iter()
        .find(|path| !path.starts_with('/'))
    {
        return Err(format!(
            "maintenance: exempt_paths: {path:?} must start with /"
        ));
    }

    for address in &table.exempt_addresses {
        let (network, prefix) = match address.split_once('/') {
            Some((network, prefix)) => (network, Some(prefix)),
            None => (address.as_str(), None),
        };

        let parsed: Option<std::net::IpAddr> = network.parse().ok();
        let bits = parsed.map_or(0, |address| if address.is_ipv4() { 32 } else { 128 });
        let prefix_ok = prefix.is_none_or(|prefix| prefix.parse::<u8>().is_ok_and(|n| n <= bits));

        if parsed.is_none() || !prefix_ok {
            return Err(format!(
                "maintenance: exempt_addresses: {address:?} is not an address or a CIDR range"
            ));
        }
    }

    if let Some(cookie) = &table.bypass_cookie
        && !matches!(cookie.split_once('='), Some((name, value)) if !name.is_empty() && !value.is_empty())
    {
        return Err("maintenance: bypass_cookie must be name=value".to_owned());
    }

    if let Some(text) = &table.retry_after
        && let Err(error) = parse_duration(text)
    {
        return Err(format!("maintenance: retry_after: {error}"));
    }

    Ok(())
}

/// An HTTP upstream (section 4.13), one of the `[upstream.NAME]` tables:
/// an unmanaged HTTP server that routes, fallbacks, traffic shares and
/// authentication sub-requests can target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Where the server listens: `host:port` entries, or `unix:/path`
    /// for a Unix socket. Requests take the addresses in turn, and one
    /// that fails is set aside for a while.
    pub addresses: Vec<String>,
    /// `http1` (the default), `h2c` (HTTP/2 in the clear, by prior
    /// knowledge) or `h2` (HTTP/2 over TLS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Connect with TLS; `h2` implies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<UpstreamTls>,
    /// Active health checks and passive ejection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<UpstreamHealth>,
    /// Honour X-Muru-Redirect from the upstream, as from an application
    /// (default false): its X-Muru-* response headers are otherwise
    /// removed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trust_muru_headers: bool,
    /// Directories a path in a trusted `X-Muru-Sendfile` response header
    /// must lie in for the edge to serve the file (section 4.9); with
    /// none every hint is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sendfile_roots: Vec<String>,
    /// A second response header honoured as `X-Muru-Sendfile` is, such
    /// as `X-Sendfile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sendfile_header: Option<String>,
    /// How long a connection may take to open, for example 5s (the
    /// default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout: Option<String>,
}

/// How an upstream's TLS connections are made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTls {
    /// A PEM file of the certificates trusted, by absolute path; the
    /// public roots without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
    /// The name the certificate must carry, sent as SNI; the address's
    /// host without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// Check the certificate (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<bool>,
}

/// An upstream's health checks (section 4.13): probes of each address,
/// and the ejection of one whose requests fail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamHealth {
    /// The path probed with a GET, healthy on 2xx or 3xx; without it a
    /// connection that opens is healthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// How often each address is probed, for example 10s (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    /// How long a probe may take, for example 2s (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Failed probes in a row before an address is unhealthy (default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unhealthy_after: Option<u32>,
    /// Successful probes in a row before it is healthy again (default 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthy_after: Option<u32>,
    /// Failed requests in a row before an address is ejected (default
    /// 3); 0 never ejects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eject_after: Option<u32>,
    /// How long an ejected address is left out before a request tries it
    /// again, for example 30s (the default); doubled each time the trial
    /// fails, up to ten times as long.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eject_for: Option<String>,
}

/// An upstream address as written: `host:port`, or `unix:/path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamAddress {
    /// A TCP address, by host name or IP address and port.
    Tcp(String, u16),
    /// A Unix socket, by absolute path.
    Unix(PathBuf),
}

/// Parse an upstream address.
pub fn parse_upstream_address(text: &str) -> Result<UpstreamAddress, String> {
    if let Some(path) = text.strip_prefix("unix:") {
        if !path.starts_with('/') {
            return Err(format!("{text:?}: a Unix socket path must be absolute"));
        }

        return Ok(UpstreamAddress::Unix(PathBuf::from(path)));
    }

    let (host, port) = text
        .rsplit_once(':')
        .ok_or_else(|| format!("{text:?} is not host:port or unix:/path"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port: u16 = port
        .parse()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| format!("{text:?}: the port must be a number from 1 to 65535"))?;

    if host.is_empty() || host.contains('/') || host.contains(char::is_whitespace) {
        return Err(format!("{text:?}: the host must be a name or an address"));
    }

    Ok(UpstreamAddress::Tcp(host.to_owned(), port))
}

/// The protocols an upstream speaks.
pub const UPSTREAM_PROTOCOLS: [&str; 3] = ["http1", "h2c", "h2"];

/// Check a release table's keys against its kind (section 9.9): a
/// managed HTTP release needs its server and command and takes none of
/// the worker settings, and a workers release takes none of the server
/// ones.
pub fn check_release_kind(table: &Release) -> Result<(), String> {
    let server_keys: [(&str, bool); 7] = [
        ("profile", table.profile.is_some()),
        ("command", !table.command.is_empty()),
        ("cwd", table.cwd.is_some()),
        ("bind", table.bind.is_some()),
        ("instances", table.instances.is_some()),
        ("readiness", table.readiness.is_some()),
        ("stop_signal", table.stop_signal.is_some()),
    ];

    if table.kind != Some(ReleaseKind::Http) {
        if let Some((key, _)) = server_keys.iter().find(|(_, set)| *set) {
            return Err(format!(
                "{key} applies to a managed HTTP release (kind = 'http')"
            ));
        }

        return Ok(());
    }

    let timeouts = table.timeouts.as_ref();
    let worker_keys: [(&str, bool); 21] = [
        ("entrypoint", !table.entrypoint.is_empty()),
        ("interface", table.interface.is_some()),
        ("factory", table.factory.is_some()),
        ("factory_phase", table.factory_phase.is_some()),
        ("python", table.python.is_some()),
        ("processes", table.processes.is_some()),
        ("threads", table.threads.is_some()),
        ("sync_threads", table.sync_threads.is_some()),
        ("concurrency", table.concurrency.is_some()),
        ("scaling", table.scaling.is_some()),
        ("preload", table.preload.is_some()),
        ("hooks", table.hooks.is_some()),
        ("groups", !table.groups.is_empty()),
        ("mounts", !table.mounts.is_empty()),
        ("logging", table.logging.is_some()),
        ("max_requests", table.max_requests.is_some()),
        ("max_lifetime", table.max_lifetime.is_some()),
        ("memory", table.memory.is_some()),
        ("accel_redirect", table.accel_redirect.is_some()),
        ("cgi_local_redirect", table.cgi_local_redirect.is_some()),
        (
            "timeouts",
            timeouts.is_some_and(|timeouts| {
                timeouts.soft_timeout.is_some()
                    || timeouts.hard_timeout.is_some()
                    || timeouts.hard_drain_grace.is_some()
                    || timeouts.loop_stall_timeout.is_some()
            }),
        ),
    ];

    if let Some((key, _)) = worker_keys.iter().find(|(_, set)| *set) {
        return Err(format!(
            "{key} does not apply to a managed HTTP release; only shutdown_timeout of the timeouts does"
        ));
    }

    let Some(profile) = table.profile else {
        return Err(
            "profile: a managed HTTP release names its server: gunicorn, uvicorn, granian, \
             mod_wsgi-express or generic"
                .to_owned(),
        );
    };

    if table.command.is_empty() {
        return Err(
            "command: a managed HTTP release needs the command that runs the server".to_owned(),
        );
    }

    if table.source.is_none() && table.cwd.is_none() {
        return Err(
            "cwd: a managed HTTP release needs the directory its command runs in".to_owned(),
        );
    }

    if table.instances == Some(0) {
        return Err("instances must be at least 1".to_owned());
    }

    let bind = table.bind.unwrap_or(ServerBind::Unix);

    if profile == ServerProfile::ModWsgiExpress && bind == ServerBind::Unix {
        return Err("bind: mod_wsgi-express binds a port only; set bind = 'port'".to_owned());
    }

    if bind == ServerBind::Unix
        && let Some(argument) = table
            .command
            .iter()
            .find(|argument| argument.contains("{port}"))
    {
        return Err(format!(
            "command: {argument:?} uses {{port}}, which needs bind = 'port'"
        ));
    }

    if bind == ServerBind::Port
        && let Some(argument) = table
            .command
            .iter()
            .find(|argument| argument.contains("{socket}"))
    {
        return Err(format!(
            "command: {argument:?} uses {{socket}}, which needs bind = 'unix'"
        ));
    }

    if let Some(readiness) = &table.readiness {
        if let Some(path) = &readiness.path
            && !path.starts_with('/')
        {
            return Err(format!("readiness.path {path:?} must start with /"));
        }

        if let Some(timeout) = &readiness.timeout {
            let duration =
                parse_duration(timeout).map_err(|error| format!("readiness.timeout: {error}"))?;

            if duration.is_zero() {
                return Err("readiness.timeout must be longer than nothing".to_owned());
            }
        }
    }

    match &table.stop_signal {
        Some(signal) if !STOP_SIGNALS.contains(&signal.as_str()) => {
            return Err(format!(
                "stop_signal must be one of {}, not {signal:?}",
                STOP_SIGNALS.join(", ")
            ));
        }
        None if profile == ServerProfile::Generic => {
            return Err(format!(
                "stop_signal: the generic profile needs the signal that stops the server: {}",
                STOP_SIGNALS.join(", ")
            ));
        }
        _ => {}
    }

    Ok(())
}

/// Check a mount's, release's or upstream's file hint keys (section
/// 4.9): absolute roots and a header name.
pub fn check_sendfile(roots: &[String], header: Option<&str>) -> Result<(), String> {
    for root in roots {
        if !root.starts_with('/') {
            return Err(format!("sendfile_roots: {root:?} must be an absolute path"));
        }
    }

    if let Some(header) = header {
        let token = !header.is_empty()
            && header
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));

        if !token {
            return Err(format!("sendfile_header: {header:?} is not a header name"));
        }

        if header.to_ascii_lowercase().starts_with("x-muru-") {
            return Err(
                "sendfile_header: X-Muru-Sendfile is always honoured; name the other header"
                    .to_owned(),
            );
        }
    }

    Ok(())
}

/// Check an `[upstream.NAME]` table: the addresses, the protocol, the
/// TLS and health settings and the durations. Whether the CA file is
/// readable is for the daemon to check.
pub fn check_upstream(table: &Upstream) -> Result<(), String> {
    if table.addresses.is_empty() {
        return Err("addresses names nowhere".to_owned());
    }

    check_sendfile(&table.sendfile_roots, table.sendfile_header.as_deref())?;

    for address in &table.addresses {
        parse_upstream_address(address).map_err(|error| format!("addresses: {error}"))?;
    }

    let protocol = table.protocol.as_deref().unwrap_or("http1");

    if !UPSTREAM_PROTOCOLS.contains(&protocol) {
        return Err(format!(
            "protocol must be http1, h2c or h2, not {protocol:?}"
        ));
    }

    if protocol == "h2c" && table.tls.is_some() {
        return Err("h2c is HTTP/2 in the clear; use h2 with tls".to_owned());
    }

    if let Some(ca) = table.tls.as_ref().and_then(|tls| tls.ca.as_deref())
        && !ca.starts_with('/')
    {
        return Err(format!("tls.ca: {ca:?} must be an absolute path"));
    }

    if let Some(text) = &table.connect_timeout
        && let Err(error) = parse_duration(text)
    {
        return Err(format!("connect_timeout: {error}"));
    }

    if let Some(health) = &table.health {
        if let Some(path) = &health.path
            && !path.starts_with('/')
        {
            return Err(format!("health.path {path:?} must start with /"));
        }

        for (name, text) in [
            ("interval", &health.interval),
            ("timeout", &health.timeout),
            ("eject_for", &health.eject_for),
        ] {
            if let Some(text) = text
                && let Err(error) = parse_duration(text)
            {
                return Err(format!("health.{name}: {error}"));
            }
        }

        for (name, count) in [
            ("unhealthy_after", health.unhealthy_after),
            ("healthy_after", health.healthy_after),
        ] {
            if count == Some(0) {
                return Err(format!("health.{name} must be at least 1"));
            }
        }
    }

    Ok(())
}

/// An application's table and its releases.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    /// Releases by identifier.
    #[serde(default)]
    pub release: BTreeMap<String, Release>,
    /// Scheduled capacity (section 7.5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule: Vec<CapacitySchedule>,
    /// The user the application's hosts and workers run as, when the
    /// daemon runs as root (section 10.9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The group, else the user's primary group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The file creation mask, in octal, for example 027.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub umask: Option<String>,
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// How the daemon itself runs (ADR 0026).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<Daemon>,
    /// Listeners.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listener: Vec<Listener>,
    /// Limits in force everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
    /// Trusted proxies and the headers taken from them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<Proxy>,
    /// How responses are compressed (section 4.17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,
    /// Custom error pages (section 4.12), by status or status:reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub error_pages: BTreeMap<String, ErrorPage>,
    /// Replace the application's own error responses with error pages
    /// (section 4.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_override: Option<ErrorOverride>,
    /// Secret stores (section 10.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Secrets>,
    /// Log rotation (section 11.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<Logs>,
    /// The metrics listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    /// Event sinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Events>,
    /// The dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<Dashboard>,
    /// The overlays' external trigger (section 9.10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_webhook: Option<OverlayWebhook>,
    /// Virtual hosts by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host: BTreeMap<String, Host>,
    /// Client classification rules, in order (section 4.14), from the
    /// daemon's own file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_class: Vec<ClientClass>,
    /// Rate limits by name (section 4.15), from the daemon's own file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rate_limit: BTreeMap<String, RateLimit>,
    /// Overlays by name (section 9.10): partial configuration laid over
    /// the routes and applications while active.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overlay: BTreeMap<String, Overlay>,
    /// Routes, in the order written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route: Vec<Route>,
    /// HTTP upstreams by name (section 4.13).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub upstream: BTreeMap<String, Upstream>,
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
        self.client_class.extend(other.client_class);

        for (name, limit) in other.rate_limit {
            if self.rate_limit.contains_key(&name) {
                return Err(format!("rate_limit {name} is declared twice"));
            }

            self.rate_limit.insert(name, limit);
        }

        for (name, overlay) in other.overlay {
            if self.overlay.contains_key(&name) {
                return Err(format!("overlay {name} is declared twice"));
            }

            self.overlay.insert(name, overlay);
        }

        if other.limits.is_some() {
            self.limits = other.limits;
        }

        if other.daemon.is_some() {
            self.daemon = other.daemon;
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

        if other.overlay_webhook.is_some() {
            self.overlay_webhook = other.overlay_webhook;
        }

        if other.proxy.is_some() {
            self.proxy = other.proxy;
        }

        if other.compression.is_some() {
            self.compression = other.compression;
        }

        if other.error_override.is_some() {
            self.error_override = other.error_override;
        }

        if other.secrets.is_some() {
            self.secrets = other.secrets;
        }

        if other.logs.is_some() {
            self.logs = other.logs;
        }

        self.error_pages.extend(other.error_pages);

        for (name, host) in other.host {
            if self.host.contains_key(&name) {
                return Err(format!("host {name} is declared twice"));
            }

            self.host.insert(name, host);
        }

        for (name, upstream) in other.upstream {
            if self.upstream.contains_key(&name) {
                return Err(format!("upstream {name} is declared twice"));
            }

            self.upstream.insert(name, upstream);
        }

        for (name, app) in other.app {
            let entry = self.app.entry(name.clone()).or_default();

            // The identity (section 10.9) comes along; a later file that
            // sets it replaces an earlier one's.

            if app.user.is_some() || app.group.is_some() || app.umask.is_some() {
                entry.user = app.user;
                entry.group = app.group;
                entry.umask = app.umask;
            }

            for (id, release) in app.release {
                if entry.release.contains_key(&id) {
                    return Err(format!("release {name}@{id} is declared twice"));
                }

                entry.release.insert(id, release);
            }

            for schedule in app.schedule {
                if entry.schedule.iter().any(|seen| seen.name == schedule.name) {
                    return Err(format!(
                        "schedule {} of {name} is declared twice",
                        schedule.name
                    ));
                }

                entry.schedule.push(schedule);
            }
        }

        Ok(())
    }
}

/// The `[tool.muru]` table: defaults an application ships with (section
/// 6.8). Every field is optional; what a command line or a release
/// definition sets takes precedence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolMuru {
    /// The application as module:callable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    /// The interface the entrypoint speaks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<Interface>,
    /// Serve gRPC server reflection for the registered services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflection: Option<bool>,
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
    /// How the default group is scaled (sections 7.2 and 7.4); static
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaling: Option<Scaling>,
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
    /// The memory policy for the workers (section 7.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Memory>,
    /// Environment variables for the workers (section 10.8): a value, or
    /// a secret reference resolved when the release starts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, EnvValue>,
    /// Treat an `X-Accel-Redirect` response header as an internal
    /// redirect (section 4.10), for the default mount and any mount that
    /// does not say otherwise (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accel_redirect: Option<bool>,
    /// Treat a 200 response with a `Location` starting with `/` and no
    /// body as an internal redirect, the CGI convention (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgi_local_redirect: Option<bool>,
    /// Directories a path in an `X-Muru-Sendfile` response header must
    /// lie in for the edge to serve the file (section 4.9), for the
    /// default mount and any mount that does not say otherwise; with
    /// none every hint is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sendfile_roots: Vec<String>,
    /// A second response header honoured as `X-Muru-Sendfile` is, such
    /// as `X-Sendfile` for the frameworks that send that one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sendfile_header: Option<String>,
    /// Custom error pages (section 4.12), by status or status:reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub error_pages: BTreeMap<String, ErrorPage>,
    /// Replace the application's own error responses with error pages
    /// (section 4.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_override: Option<ErrorOverride>,
    /// What maintenance mode lets through (section 4.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<Maintenance>,
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

    fn upload_route(keys: &str) -> Route {
        toml::from_str(&format!("host = 'shop'\napp = 'shop'\n{keys}")).unwrap()
    }

    #[test]
    fn upload_settings_resolve_with_their_defaults() {
        let settings = check_upload(&upload_route("upload = 'raw'"))
            .unwrap()
            .unwrap();
        assert_eq!(settings.mode, "raw");
        assert_eq!(settings.max_file_size, None);
        assert_eq!(settings.max_files, DEFAULT_MAX_FILES);
        assert_eq!(settings.hash, vec!["sha256".to_owned()]);
        assert!(!settings.precheck);
        assert_eq!(settings.spool_quota, DEFAULT_SPOOL_QUOTA);

        let settings = check_upload(&upload_route(
            "upload = 'multipart'\nmax_file_size = '5MB'\nhash = ['sha512', 'sha1']\nspool_quota = '1GB'",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(settings.max_file_size, Some(5 * 1024 * 1024));
        assert_eq!(settings.hash, vec!["sha512".to_owned(), "sha1".to_owned()]);
        assert_eq!(settings.spool_quota, 1024 * 1024 * 1024);
        assert_eq!(check_upload(&upload_route("")).unwrap(), None);
    }

    #[test]
    fn moments_are_read_in_their_time_zone() {
        // 09:00 in Sydney in November is 22:00 UTC the day before.

        assert_eq!(
            parse_moment("2026-11-02T09:00", Some("Australia/Sydney")),
            Ok(1_793_570_400_000)
        );
        assert_eq!(
            parse_moment("2026-11-01T22:00:00Z", Some("Australia/Sydney")),
            Ok(1_793_570_400_000)
        );

        // 02:30 does not exist on the morning clocks go forward in New
        // York; it is read as 03:30.

        assert_eq!(
            parse_moment("2026-03-08T02:30", Some("America/New_York")),
            parse_moment("2026-03-08T03:30", Some("America/New_York"))
        );
        assert!(parse_moment("2026-11-02T09:00", Some("Mars/Olympus")).is_err());
        assert!(parse_moment("tomorrow", None).is_err());
    }

    #[test]
    fn overlays_are_checked() {
        let overlay = |text: &str| -> Overlay { toml::from_str(text).unwrap() };
        assert!(check_overlay(
            "backup",
            &overlay("routes.shop-root = { mode = 'read-only', deny = ['POST'], error_pages = { '503:read_only' = { template = '<p>Back at 3</p>' } } }\ntrigger = { schedule = { start = '2026-11-02T02:00', end = '2026-11-02T03:00', timezone = 'UTC' } }")
        )
        .is_ok());
        assert!(check_overlay(
            "crawler-surge",
            &overlay("routes.shop-root.client_routes = { ai_crawler = { target = { static = '/srv/shop/snapshot' } } }")
        )
        .is_ok());
        assert!(check_overlay(
            "nightly",
            &overlay("apps.shop = { maintenance = true }\ntrigger = { schedule = { cron = '0 2 * * *', duration = '1h', timezone = 'Australia/Sydney' } }\nwarm_up = { lead = '30m' }\nadvisory = true")
        )
        .is_ok());

        for (text, expected) in [
            ("priority = 1", "changes no route"),
            ("routes.a = { deny = ['post'] }", "not a method"),
            ("routes.a = { answer = 'read_only' }", "answer needs deny"),
            ("routes.a = { traffic = {} }", "one of pin and split"),
            ("routes.a = { error_pages = {} }", "names no page"),
            ("routes.a = { client_routes = {} }", "names no class"),
            (
                "routes.a = { client_routes = { bot = {} } }",
                "bot sets nothing",
            ),
            (
                "routes.a = { client_routes = { 'Bot' = { mode = 'read-only' } } }",
                "client_routes",
            ),
            (
                "routes.a = { client_routes = { bot = { client_routes = { x = { mode = 'read-only' } } } } }",
                "not on a client class's",
            ),
            (
                "routes.a = { client_routes = { bot = { target = {} } } }",
                "client_routes.bot: target names one of",
            ),
            (
                "routes.a = { error_pages = { '503' = { file = 'down.html' } } }",
                "absolute",
            ),
            (
                "routes.a = { target = { app = 'x', upstream = 'y' } }",
                "one of app, static",
            ),
            (
                "apps.shop = { maintenance = true }\ntrigger = { schedule = { start = 'soon' } }",
                "schedule: start",
            ),
            (
                "apps.shop = { maintenance = true }\ntrigger = { schedule = { cron = '0 2 * * *' } }",
                "cron needs a duration",
            ),
            (
                "apps.shop = { maintenance = true }\ntrigger = { schedule = { cron = '0 2 * * *', start = '2026-11-02T09:00' } }",
                "one of start and cron",
            ),
            (
                "apps.shop = { maintenance = true }\ntrigger = { schedule = { cron = '0 25 * * *', duration = '1h' } }",
                "hour \"25\"",
            ),
            (
                "apps.shop = { maintenance = true }\nwarm_up = { lead = '30m' }",
                "warm_up needs a schedule",
            ),
            (
                "apps.shop = { maintenance = true }\nlatch = true",
                "latch and advisory apply to a schedule",
            ),
            ("apps.shop = {}", "apps.shop: sets nothing"),
            ("apps.shop = { min_workers = 0 }", "at least 1"),
            (
                "apps.shop = { min_workers = 4, max_workers = 2 }",
                "below min_workers",
            ),
            ("apps.shop = { scale_in = 'never' }", "not hold or normal"),
            (
                "apps.shop = { group = 'reports', maintenance = true }",
                "none is set",
            ),
            (
                "apps.shop = { maintenance = true }\ntrigger = { schedule = { start = '2026-11-02T09:00', end = '2026-11-02T08:00', timezone = 'UTC' } }",
                "end must be after",
            ),
        ] {
            let error = check_overlay("o", &overlay(text)).unwrap_err();
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn capacity_schedules_are_overlays_and_back() {
        let document = Document::parse(
            "[[app.shop.schedule]]\nname = 'black-friday'\nstart = '2026-11-27T00:00'\nend = '2026-11-30T23:59'\ntimezone = 'America/New_York'\nmin_workers = 24\nmax_workers = 48\nwarm_up = '30m'\nscale_in = 'hold'\nfreeze_deploys = true\n",
        )
        .unwrap();
        let schedule = &document.app["shop"].schedule[0];
        let overlay = schedule.overlay("shop");
        assert!(check_overlay("shop.black-friday", &overlay).is_ok());
        assert_eq!(overlay.apps["shop"].min_workers, Some(24));
        assert_eq!(overlay.warm_up.as_ref().unwrap().lead, "30m");
        assert_eq!(
            CapacitySchedule::from_overlay("shop.black-friday", &overlay),
            Some(("shop".to_owned(), schedule.clone()))
        );
        assert_eq!(CapacitySchedule::from_overlay("backup", &overlay), None);

        let mut merged = document.clone();
        let error = merged.merge(document).unwrap_err();
        assert!(error.contains("schedule black-friday of shop is declared twice"));
    }

    #[test]
    fn rates_are_parsed() {
        assert_eq!(parse_rate("2/s"), Ok((2, 1)));
        assert_eq!(parse_rate("100 / m"), Ok((100, 60)));
        assert_eq!(parse_rate("1000/hour"), Ok((1000, 3600)));

        for bad in ["2", "0/s", "2/d", "x/s"] {
            assert!(parse_rate(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rate_limits_resolve_with_their_defaults() {
        let limit: RateLimit = toml::from_str("rate = '120/m'").unwrap();
        let settings = check_rate_limit("crawlers", &limit).unwrap();
        assert_eq!(settings.key, vec!["address".to_owned()]);
        assert!((settings.per_second - 2.0).abs() < f64::EPSILON);
        assert_eq!(settings.burst, 120);
        assert_eq!(settings.action, "reject");
        assert_eq!((settings.ipv4_prefix, settings.ipv6_prefix), (32, 64));

        let limit: RateLimit = toml::from_str(
            "rate = '2/s'\nburst = 10\nkey = ['client_class', 'header:x-api-key']\naction = 'delay'\nmax_delay = '500ms'\nmatch = { client_class = ['ai_crawler'] }",
        )
        .unwrap();
        let settings = check_rate_limit("crawlers", &limit).unwrap();
        assert_eq!(settings.classes, vec!["ai_crawler".to_owned()]);
        assert_eq!(settings.key.len(), 2);
        assert_eq!(settings.max_delay, std::time::Duration::from_millis(500));

        for (text, expected) in [
            ("rate = '2/s'\nkey = 'path'", "key \"path\""),
            ("rate = '2/s'\naction = 'drop'", "action must be"),
            ("rate = '2/s'\nmax_delay = '1s'", "max_delay needs"),
            ("rate = '2/s'\nipv6_prefix = 0", "ipv6_prefix"),
            ("rate = 'often'", "rate:"),
        ] {
            let limit: RateLimit = toml::from_str(text).unwrap();
            let error = check_rate_limit("crawlers", &limit).unwrap_err();
            assert!(error.contains(expected), "{text}: {error}");
        }

        assert!(check_rate_limit("Crawlers", &RateLimit::default()).is_err());
    }

    #[test]
    fn client_classes_are_checked() {
        let class = |text: &str| -> ClientClass { toml::from_str(text).unwrap() };
        assert!(
            check_client_class(&class("name = 'partner'\naddresses = ['203.0.113.0/24']")).is_ok()
        );

        for (text, expected) in [
            ("name = 'partner'", "names nothing"),
            (
                "name = 'default'\nuser_agent = ['x']",
                "default is the class",
            ),
            (
                "name = 'bot'\nuser_agent_regex = ['(']",
                "not a regular expression",
            ),
            (
                "name = 'bot'\nuser_agent = ['x']\nverify = 'dns'",
                "needs verify_domains",
            ),
            (
                "name = 'bot'\nuser_agent = ['x']\nverify_domains = ['a.com']",
                "needs verify = 'dns'",
            ),
            (
                "name = 'bot'\nuser_agent = ['x']\nverify = 'whois'\nverify_domains = ['a.com']",
                "verify must be dns",
            ),
        ] {
            let error = check_client_class(&class(text)).unwrap_err();
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn route_policies_are_checked() {
        let route = |keys: &str| upload_route(keys);
        assert!(
            check_route_policies(&route(
                "rate_limits = ['crawlers']\nmode = 'read-only'\nmethods = { deny = ['POST'] }"
            ))
            .is_ok()
        );

        for (keys, expected) in [
            ("methods = { deny = [] }", "names no method"),
            ("methods = { deny = ['post'] }", "not a method"),
            (
                "methods = { deny = ['POST'], answer = 'teapot' }",
                "methods.answer",
            ),
            ("mode = 'read only'", "mode:"),
            ("rate_limits = ['Bad']", "rate_limits:"),
            ("match = { client_class = [] }", "names no class"),
            ("match = { client_class = ['AI'] }", "match.client_class:"),
            (
                "traffic = { classes = { 'Bots' = 'r1' } }",
                "traffic.classes:",
            ),
        ] {
            let error = check_route_policies(&route(keys)).unwrap_err();
            assert!(error.contains(expected), "{keys}: {error}");
        }
    }

    #[test]
    fn routes_for_classes_have_keys_of_their_own() {
        let route = |keys: &str| upload_route(keys);
        assert_eq!(route("").key(), "/");
        assert_eq!(
            route("match = { client_class = ['partner', 'ai_crawler', 'partner'] }").key(),
            "/ client_class=ai_crawler,partner"
        );
        assert!(
            check_route_policies(&route(
                "match = { client_class = ['default'] }
traffic = { classes = { ai_crawler = 'r1' } }"
            ))
            .is_ok()
        );
    }

    #[test]
    fn upload_keys_are_checked() {
        for (keys, expected) in [
            ("upload = 'stream'", "upload must be multipart or raw"),
            ("precheck = true", "precheck needs upload"),
            ("upload = 'raw'\nmax_files = 0", "max_files must be from 1"),
            (
                "upload = 'raw'\nmax_files = 501",
                "max_files must be from 1",
            ),
            ("upload = 'raw'\nhash = ['md5']", "hash: \"md5\""),
            ("upload = 'raw'\nmax_file_size = 'big'", "max_file_size:"),
            ("max_body_size = 'lots'", "max_body_size:"),
        ] {
            let error = check_upload(&upload_route(keys)).unwrap_err();
            assert!(error.contains(expected), "{keys}: {error}");
        }
    }

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
                        scaling: Some(Scaling {
                            mode: Some(ScalingMode::Spare),
                            max_workers: Some(8),
                            ..Scaling::default()
                        }),
                        ..Release::default()
                    },
                )]),
                ..App::default()
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
            name: Some("shop-root".to_owned()),
            host: "shop".to_owned(),
            path: "/".to_owned(),
            matches: None,
            app: "shop".to_owned(),
            upstream: String::new(),
            static_dir: None,
            index: Vec::new(),
            fallback: None,
            cache: None,
            redirect_slash: None,
            case: None,
            internal: None,
            accept_redirects_from: Vec::new(),
            error_pages: BTreeMap::new(),
            error_override: None,
            auth: None,
            traffic: Some(Traffic {
                split: BTreeMap::from([("r2".to_owned(), 10)]),
                sticky: Some(true),
                ..Traffic::default()
            }),
            max_body_size: Some("1GB".to_owned()),
            upload: Some("multipart".to_owned()),
            max_file_size: None,
            max_files: Some(5),
            hash: Some(Vec::new()),
            precheck: Some(true),
            spool_quota: None,
            rate_limits: vec!["crawlers".to_owned()],
            methods: Some(Methods {
                deny: vec!["POST".to_owned()],
                answer: None,
            }),
            mode: Some("read-only".to_owned()),
        });
        let text = document.to_toml().unwrap();
        assert!(text.contains("name = \"shop-root\""), "{text}");
        assert!(text.contains("rate_limits = [\"crawlers\"]"), "{text}");
        assert!(text.contains("mode = \"read-only\""), "{text}");
        assert!(text.contains("upload = \"multipart\""), "{text}");
        assert!(text.contains("hash = []"), "{text}");
        assert!(text.contains("[app.shop.release.r1]"), "{text}");
        assert!(text.contains("[app.shop.release.r1.scaling]"), "{text}");
        assert!(text.contains("mode = \"spare\""), "{text}");
        assert!(text.contains("[route.traffic]"), "{text}");
        assert!(text.contains("r2 = 10"), "{text}");
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
    fn upstream_tables_are_checked() {
        let document = Document::parse(
            "[upstream.legacy]\naddresses = ['127.0.0.1:8080', 'unix:/run/legacy.sock']\nprotocol = 'h2c'\nhealth = { path = '/healthz', interval = '5s' }\n",
        )
        .unwrap();
        let legacy = &document.upstream["legacy"];
        assert_eq!(check_upstream(legacy), Ok(()));
        assert_eq!(
            parse_upstream_address("unix:/run/legacy.sock"),
            Ok(UpstreamAddress::Unix(PathBuf::from("/run/legacy.sock")))
        );
        assert_eq!(
            parse_upstream_address("[::1]:8080"),
            Ok(UpstreamAddress::Tcp("::1".to_owned(), 8080))
        );
        let refused = |text: &str| check_upstream(&toml::from_str(text).unwrap()).unwrap_err();
        assert!(refused("addresses = []").contains("nowhere"));
        assert!(refused("addresses = ['nowhere']").contains("host:port"));
        assert!(refused("addresses = ['unix:relative']").contains("absolute"));
        assert!(refused("addresses = ['a:0']").contains("port"));
        assert!(refused("addresses = ['a:1']\nprotocol = 'spdy'").contains("http1, h2c or h2"));
        assert!(refused("addresses = ['a:1']\nprotocol = 'h2c'\ntls = {}").contains("h2 with tls"));
        assert!(refused("addresses = ['a:1']\ntls = { ca = 'ca.pem' }").contains("absolute"));
        assert!(refused("addresses = ['a:1']\nhealth = { path = 'x' }").contains("start with /"));
        assert!(
            refused("addresses = ['a:1']\nhealth = { unhealthy_after = 0 }").contains("at least 1")
        );
        assert!(
            refused("addresses = ['a:1']\nconnect_timeout = 'soon'").contains("connect_timeout")
        );
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
