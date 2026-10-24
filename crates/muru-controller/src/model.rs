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
