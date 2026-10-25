//! The objects the controller manages: applications and releases (design
//! specification sections 3 and 9.1). The first slice holds what a deploy
//! of one application needs; routes, hosts and the rest arrive with their
//! features.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use muru_config::{FactoryPhase, Interface, Preload};
use serde::{Deserialize, Serialize};

/// The states of section 9.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseState {
    /// Recorded, not yet started.
    Pending,
    /// The host is starting and preloading.
    Starting,
    /// Workers are forking and not yet all ready.
    Warming,
    /// Every worker is ready; the release can take traffic.
    Ready,
    /// Removed from traffic; requests in flight are finishing.
    Draining,
    /// Drained and kept warm for the retention window, so a rollback to it
    /// is instant (section 9.1).
    Retained,
    /// Hosts and workers are being stopped.
    Stopping,
    /// Stopped, and can be started again (a rollback target).
    Stopped,
    /// Failed to start or to stay up; `message` says why.
    Failed,
}

impl ReleaseState {
    /// Every state, for parsing.
    pub const ALL: [ReleaseState; 9] = [
        ReleaseState::Pending,
        ReleaseState::Starting,
        ReleaseState::Warming,
        ReleaseState::Ready,
        ReleaseState::Draining,
        ReleaseState::Retained,
        ReleaseState::Stopping,
        ReleaseState::Stopped,
        ReleaseState::Failed,
    ];

    /// The name as the API and the CLI show it.
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseState::Pending => "pending",
            ReleaseState::Starting => "starting",
            ReleaseState::Warming => "warming",
            ReleaseState::Ready => "ready",
            ReleaseState::Draining => "draining",
            ReleaseState::Retained => "retained",
            ReleaseState::Stopping => "stopping",
            ReleaseState::Stopped => "stopped",
            ReleaseState::Failed => "failed",
        }
    }

    /// Whether the release has processes: a host that may serve or drain.
    pub fn is_running(self) -> bool {
        matches!(
            self,
            ReleaseState::Starting
                | ReleaseState::Warming
                | ReleaseState::Ready
                | ReleaseState::Draining
                | ReleaseState::Retained
                | ReleaseState::Stopping
        )
    }

    /// Whether nothing further happens to the release on its own.
    pub fn is_final(self) -> bool {
        matches!(self, ReleaseState::Stopped | ReleaseState::Failed)
    }
}

impl fmt::Display for ReleaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReleaseState {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        ReleaseState::ALL
            .into_iter()
            .find(|state| state.as_str() == text)
            .ok_or_else(|| format!("{text:?} is not a release state"))
    }
}

/// A mount of a release (section 6.7): a path served by another
/// entrypoint in the same workers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    /// The mount's name.
    pub name: String,
    /// The path prefix.
    pub path: String,
    /// The application as module:callable.
    pub entrypoint: String,
    /// The interface, auto when detected.
    pub interface: Interface,
}

/// A worker group of a release (section 7.1): its paths and sizes. The
/// default group is the release's own settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSpec {
    /// The group's name.
    pub name: String,
    /// Path prefixes it serves, with an optional trailing `*`.
    pub paths: Vec<String>,
    /// Worker processes.
    pub processes: u32,
    /// Application threads per worker.
    pub threads: u32,
    /// Threads for synchronous code called from an ASGI application.
    pub sync_threads: u32,
    /// Requests an ASGI worker's event loop takes at once.
    pub concurrency: u32,
}

/// What a release runs: its source and the settings fixed at creation
/// (ADR 0003). Scale is the one thing that changes live, later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseSpec {
    /// The source directory.
    pub source: PathBuf,
    /// The application as module:callable.
    pub entrypoint: String,
    /// The interface, auto when detected.
    pub interface: Interface,
    /// The interpreter.
    pub python: PathBuf,
    /// Worker processes.
    pub processes: u32,
    /// Application threads per worker.
    pub threads: u32,
    /// Threads for synchronous code called from an ASGI application.
    pub sync_threads: u32,
    /// Requests an ASGI worker's loop takes at once.
    pub concurrency: u32,
    /// The preload mode.
    pub preload: Preload,
    /// The hooks module, if any.
    pub hooks: Option<String>,
    /// Whether the entrypoint is a factory, and when it is called.
    pub factory: Option<FactoryPhase>,
    /// Named worker groups beside the default one.
    #[serde(default)]
    pub groups: Vec<GroupSpec>,
    /// Named mounts beside the default one at /.
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    /// Python logging defaults (section 11.4).
    #[serde(default)]
    pub logging: muru_config::PythonLogging,
    /// Requests a worker serves before it is replaced (section 7.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    /// How long a worker lives before it is replaced, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime_ms: Option<u64>,
    /// The timeouts the host enforces on its workers (section 7.6).
    #[serde(default)]
    pub timeouts: WorkerTimeouts,
    /// The memory policy for the workers (section 7.7).
    #[serde(default)]
    pub memory: MemoryPolicy,
    /// Environment variables for the workers (section 10.8), in name
    /// order; secret values are never recorded, only their references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvSpec>,
}

/// One environment variable of a release (section 10.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvSpec {
    /// The variable's name.
    pub name: String,
    /// Its value as written, when it is not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The store a secret value comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_store: Option<String>,
    /// The key of the secret in its store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    /// Whether the secret is delivered as a file named by NAME_FILE.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub as_file: bool,
}

impl EnvSpec {
    /// The variable as a phrase that never shows a secret's value.
    pub fn describe(&self) -> String {
        match (&self.secret_store, &self.secret_key) {
            (Some(store), Some(key)) => format!(
                "{}=secret {store}/{key}{}",
                self.name,
                if self.as_file { " as file" } else { "" }
            ),
            _ => format!("{}={}", self.name, self.value.clone().unwrap_or_default()),
        }
    }
}

/// The memory policy of a release's workers (section 7.7), in bytes and
/// milliseconds; the defaults are those of appendix C.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPolicy {
    /// What is measured.
    #[serde(default = "default_measure")]
    pub measure: muru_config::Measure,
    /// Over this for `soft_for_ms`, the worker reclaims and then
    /// recycles; off when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_limit: Option<u64>,
    /// How long a worker may stay over the soft limit.
    #[serde(default = "default_soft_for_ms")]
    pub soft_for_ms: u64,
    /// Whether to reclaim before recycling over the soft limit.
    #[serde(default = "default_true")]
    pub reclaim_first: bool,
    /// Over this the worker leaves routing at once and is replaced; off
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_limit: Option<u64>,
    /// How often memory is measured.
    #[serde(default = "default_check_interval_ms")]
    pub check_interval_ms: u64,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            measure: default_measure(),
            soft_limit: None,
            soft_for_ms: default_soft_for_ms(),
            reclaim_first: true,
            hard_limit: None,
            check_interval_ms: default_check_interval_ms(),
        }
    }
}

fn default_measure() -> muru_config::Measure {
    muru_config::Measure::Uss
}

fn default_soft_for_ms() -> u64 {
    120_000
}

fn default_true() -> bool {
    true
}

fn default_check_interval_ms() -> u64 {
    5_000
}

/// The timeouts a host enforces on its workers (section 7.6), in
/// milliseconds; the defaults are those of appendix C.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTimeouts {
    /// How long a request may run before its worker is replaced; off
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_timeout_ms: Option<u64>,
    /// How long a replaced worker gets to finish its other requests.
    #[serde(default = "default_hard_drain_grace_ms")]
    pub hard_drain_grace_ms: u64,
    /// How long a worker told to stop gets before it is killed.
    #[serde(default = "default_shutdown_timeout_ms")]
    pub shutdown_timeout_ms: u64,
}

impl Default for WorkerTimeouts {
    fn default() -> Self {
        Self {
            hard_timeout_ms: None,
            hard_drain_grace_ms: default_hard_drain_grace_ms(),
            shutdown_timeout_ms: default_shutdown_timeout_ms(),
        }
    }
}

fn default_hard_drain_grace_ms() -> u64 {
    10_000
}

fn default_shutdown_timeout_ms() -> u64 {
    5_000
}

/// A release record: its spec, where it is in section 9.1, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The application.
    pub app: String,
    /// The release id, unique within the application.
    pub id: String,
    /// The spec.
    pub spec: ReleaseSpec,
    /// Where it is.
    pub state: ReleaseState,
    /// Why it failed, or what it is doing.
    pub message: String,
    /// When it was created, Unix milliseconds.
    pub created_unix_ms: u64,
    /// For optimistic concurrency.
    pub resource_version: u64,
}

/// An application: a name and which of its releases serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// The name.
    pub name: String,
    /// The release traffic goes to, if any.
    pub current: Option<String>,
    /// The release a rollback restores, if any.
    pub previous: Option<String>,
    /// When it was created, Unix milliseconds.
    pub created_unix_ms: u64,
    /// For optimistic concurrency.
    pub resource_version: u64,
    /// The user its hosts and workers run as, when the daemon is root
    /// (section 10.9).
    pub user: Option<String>,
    /// The group, else the user's primary group.
    pub group: Option<String>,
    /// The file creation mask, in octal.
    pub umask: Option<String>,
}

impl App {
    /// The identity as one phrase, or none.
    pub fn describe_identity(&self) -> Option<String> {
        let user = self.user.as_ref()?;
        let mut text = user.clone();

        if let Some(group) = &self.group {
            text.push(':');
            text.push_str(group);
        }

        if let Some(umask) = &self.umask {
            text.push_str(&format!(" umask {umask}"));
        }

        Some(text)
    }
}

/// Whether a string is an octal file creation mask such as 027.
pub fn valid_umask(text: &str) -> bool {
    !text.is_empty() && text.len() <= 4 && text.chars().all(|c| ('0'..='7').contains(&c))
}

/// One state change (section 11.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The node-wide sequence number.
    pub sequence: u64,
    /// When, Unix milliseconds.
    pub time_unix_ms: u64,
    /// For example release.ready.
    pub kind: String,
    /// What it is about: app, or app@release.
    pub subject: String,
    /// info, notice, warning or error.
    pub level: &'static str,
    /// The sentence.
    pub message: String,
}

/// Check a name against section 3.2: lowercase letters, digits and
/// hyphens, up to 63 characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Whether a string is an environment variable name: letters, digits and
/// underscores, not starting with a digit.
pub fn valid_env_name(name: &str) -> bool {
    let mut characters = name.chars();

    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }

    characters.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Now, as Unix milliseconds.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_through_their_names() {
        for state in ReleaseState::ALL {
            assert_eq!(state.as_str().parse::<ReleaseState>(), Ok(state));
        }

        assert!("running".parse::<ReleaseState>().is_err());
    }

    #[test]
    fn names_follow_the_rules() {
        assert!(valid_name("shop"));
        assert!(valid_name("shop-2"));
        assert!(!valid_name("Shop"));
        assert!(!valid_name("-shop"));
        assert!(!valid_name(""));
        assert!(!valid_name("a".repeat(64).as_str()));
    }
}
