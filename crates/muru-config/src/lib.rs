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

words!(Interface { Auto => "auto", Wsgi => "wsgi", Asgi => "asgi" });
words!(Preload { Auto => "auto", True => "true", False => "false" });
words!(Measure { Uss => "uss", Pss => "pss", Rss => "rss" });
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
    /// The path prefix; the longest wins, on segment boundaries.
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
}

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

/// Check an `[upstream.NAME]` table: the addresses, the protocol, the
/// TLS and health settings and the durations. Whether the CA file is
/// readable is for the daemon to check.
pub fn check_upstream(table: &Upstream) -> Result<(), String> {
    if table.addresses.is_empty() {
        return Err("addresses names nowhere".to_owned());
    }

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
    /// Virtual hosts by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host: BTreeMap<String, Host>,
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

        if other.proxy.is_some() {
            self.proxy = other.proxy;
        }

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
            host: "shop".to_owned(),
            path: "/".to_owned(),
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
        });
        let text = document.to_toml().unwrap();
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
