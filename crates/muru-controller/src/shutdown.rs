//! What SIGTERM and SIGINT mean to the daemon, and how a shutdown runs
//! (design specification section 13.3). Under systemd a stop signal
//! detaches the daemon, so a restart never drops traffic; as a container's
//! main process it must shut everything down, since the kernel kills the
//! rest of the PID namespace when PID 1 exits. The `[shutdown]` table of
//! the daemon's file says which, with a delay in which connections are
//! closed after their response while the edge keeps accepting, and a
//! budget for the whole shutdown; the defaults suit `docker stop`'s ten
//! seconds.

use std::time::Duration;

/// Set in the environment of the command the built-in init runs for a
/// container's PID 1 (section 6.10), so the daemon knows what it is.
pub const UNDER_INIT: &str = "MURU_UNDER_INIT";

/// How long the whole shutdown may take unless the table says otherwise:
/// under the ten seconds `docker stop` gives before SIGKILL.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(8);

/// What the drain of the requests in flight leaves of the budget for the
/// workers and the edge, however long the requests run.
pub const WORKERS_RESERVE: Duration = Duration::from_secs(1);

/// Whether this process is a container's main process: PID 1 itself, or
/// the command the built-in init runs for PID 1.
pub fn is_main_process() -> bool {
    std::process::id() == 1 || std::env::var_os(UNDER_INIT).is_some()
}

/// What a stop signal makes the daemon do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnSigterm {
    /// Drain and stop every release and the edge, then exit.
    Shutdown,
    /// Exit, leaving the hosts and the edge serving for the next daemon.
    Detach,
}

/// The `[shutdown]` table, checked, with the defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// What SIGTERM and SIGINT mean.
    pub on_sigterm: OnSigterm,
    /// How long the edge keeps accepting after the shutdown begins, with
    /// every connection closing after its response.
    pub delay: Duration,
    /// How long the whole shutdown may take; what has not finished by
    /// then is killed.
    pub budget: Duration,
}

impl Policy {
    /// The table's settings, or the defaults: a stop signal shuts a
    /// container's main process down and detaches any other daemon, with
    /// no delay and the default budget.
    pub fn from_table(
        table: Option<&muru_config::Shutdown>,
        main_process: bool,
    ) -> Result<Self, String> {
        let on_sigterm = match table.and_then(|table| table.on_sigterm.as_deref()) {
            None if main_process => OnSigterm::Shutdown,
            None => OnSigterm::Detach,
            Some("shutdown") => OnSigterm::Shutdown,
            Some("detach") => OnSigterm::Detach,
            Some(other) => {
                return Err(format!(
                    "on_sigterm = {other:?} is neither 'shutdown' nor 'detach'"
                ));
            }
        };

        let delay = match table.and_then(|table| table.delay.as_deref()) {
            Some(text) => {
                muru_config::parse_duration(text).map_err(|error| format!("delay: {error}"))?
            }
            None => Duration::ZERO,
        };

        let budget = match table.and_then(|table| table.budget.as_deref()) {
            Some(text) => {
                muru_config::parse_duration(text).map_err(|error| format!("budget: {error}"))?
            }
            None => DEFAULT_BUDGET,
        };

        if budget.is_zero() {
            return Err("budget must be longer than nothing".to_owned());
        }

        if delay >= budget {
            return Err(format!(
                "delay = {} leaves nothing of budget = {} for the drain",
                muru_config::format_duration(delay),
                muru_config::format_duration(budget)
            ));
        }

        Ok(Self {
            on_sigterm,
            delay,
            budget,
        })
    }

    /// Whether a stop signal shuts the daemon down.
    pub fn shuts_down(&self) -> bool {
        self.on_sigterm == OnSigterm::Shutdown
    }

    /// The policy in a sentence, for the log and the reload's report.
    pub fn describe(&self) -> String {
        format!(
            "SIGTERM {}, delay {}, budget {}",
            match self.on_sigterm {
                OnSigterm::Shutdown => "shuts down",
                OnSigterm::Detach => "detaches",
            },
            seconds(self.delay),
            muru_config::format_duration(self.budget)
        )
    }
}

/// A duration as the file writes it, with none as 0s rather than 0ms.
fn seconds(duration: Duration) -> String {
    if duration.is_zero() {
        "0s".to_owned()
    } else {
        muru_config::format_duration(duration)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{OnSigterm, Policy};

    fn table(
        on_sigterm: Option<&str>,
        delay: Option<&str>,
        budget: Option<&str>,
    ) -> muru_config::Shutdown {
        muru_config::Shutdown {
            on_sigterm: on_sigterm.map(str::to_owned),
            delay: delay.map(str::to_owned),
            budget: budget.map(str::to_owned),
        }
    }

    #[test]
    fn the_default_depends_on_being_the_main_process() {
        let elsewhere = Policy::from_table(None, false).unwrap();
        assert_eq!(elsewhere.on_sigterm, OnSigterm::Detach);
        assert!(!elsewhere.shuts_down());
        assert_eq!(elsewhere.delay, Duration::ZERO);
        assert_eq!(elsewhere.budget, Duration::from_secs(8));

        let container = Policy::from_table(None, true).unwrap();
        assert_eq!(container.on_sigterm, OnSigterm::Shutdown);
        assert!(container.shuts_down());

        // An empty table is the same as none.

        let empty = table(None, None, None);
        assert_eq!(Policy::from_table(Some(&empty), true).unwrap(), container);
    }

    #[test]
    fn the_table_overrides_the_default_either_way() {
        let shutdown = table(Some("shutdown"), Some("2s"), Some("30s"));
        let policy = Policy::from_table(Some(&shutdown), false).unwrap();
        assert_eq!(policy.on_sigterm, OnSigterm::Shutdown);
        assert_eq!(policy.delay, Duration::from_secs(2));
        assert_eq!(policy.budget, Duration::from_secs(30));
        assert_eq!(
            policy.describe(),
            "SIGTERM shuts down, delay 2s, budget 30s"
        );

        let detach = table(Some("detach"), None, None);
        let policy = Policy::from_table(Some(&detach), true).unwrap();
        assert_eq!(policy.on_sigterm, OnSigterm::Detach);
        assert_eq!(policy.describe(), "SIGTERM detaches, delay 0s, budget 8s");
    }

    #[test]
    fn bad_values_are_refused_with_the_key_named() {
        let cases = [
            (table(Some("drain"), None, None), "on_sigterm"),
            (table(None, Some("soon"), None), "delay"),
            (table(None, None, Some("0s")), "budget must be longer"),
            (table(None, Some("8s"), Some("8s")), "leaves nothing"),
            (table(None, Some("9s"), None), "leaves nothing"),
        ];

        for (table, expected) in cases {
            let error = Policy::from_table(Some(&table), false).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }
}
