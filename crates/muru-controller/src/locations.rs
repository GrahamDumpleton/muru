//! Where the daemon keeps things (design specification section 13.8).
//!
//! Muru has no fixed paths. Each location is resolved from the first of:
//! an explicit option, an environment variable, a directory systemd
//! provides, or the default for the mode the daemon runs in.

use std::env;
use std::path::PathBuf;

/// The mode the daemon runs in, which decides the default locations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Root, or `--system`: the system locations.
    System,
    /// A user's own daemon: the user locations.
    User,
}

/// The explicit options a command line may give.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The daemon configuration file.
    pub config: Option<PathBuf>,
    /// The state directory.
    pub state_dir: Option<PathBuf>,
    /// The runtime directory.
    pub run_dir: Option<PathBuf>,
    /// The log directory.
    pub log_dir: Option<PathBuf>,
    /// The cache directory.
    pub cache_dir: Option<PathBuf>,
    /// The system locations, whoever runs the daemon.
    pub system: bool,
}

/// One resolved location and the rule that chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The path.
    pub path: PathBuf,
    /// Which rule of section 13.8 chose it: option, environment, systemd or default.
    pub rule: &'static str,
}

/// Every location the daemon uses.
#[derive(Debug, Clone)]
pub struct Locations {
    /// The mode the defaults came from.
    pub mode: Mode,
    /// The daemon configuration file, which need not exist.
    pub config: Location,
    /// The state store, built environments and other durable files.
    pub state: Location,
    /// The admin socket and per-release run directories.
    pub run: Location,
    /// Log files, when file sinks are configured.
    pub logs: Location,
    /// Re-creatable files.
    pub cache: Location,
}

impl Locations {
    /// Resolve every location for the given options and environment.
    pub fn resolve(options: &Options) -> Self {
        let mode = if options.system || is_root() {
            Mode::System
        } else {
            Mode::User
        };
        let home = env::var_os("HOME").map(PathBuf::from);

        let state = pick(
            options.state_dir.as_ref(),
            "MURU_STATE_DIR",
            "STATE_DIRECTORY",
            || default_state(mode, home.as_deref()),
        );
        let config = pick(
            options.config.as_ref(),
            "MURU_CONFIG",
            "CONFIGURATION_DIRECTORY",
            || default_config(mode, home.as_deref()),
        );
        let run = pick(
            options.run_dir.as_ref(),
            "MURU_RUN_DIR",
            "RUNTIME_DIRECTORY",
            || default_run(mode),
        );
        let logs = pick(
            options.log_dir.as_ref(),
            "MURU_LOG_DIR",
            "LOGS_DIRECTORY",
            || default_logs(mode, home.as_deref()),
        );
        let cache = pick(
            options.cache_dir.as_ref(),
            "MURU_CACHE_DIR",
            "CACHE_DIRECTORY",
            || default_cache(mode, home.as_deref()),
        );

        // A configuration directory holds the file; a file is used as is.

        let config = if config.path.is_dir() || config.rule == "systemd" {
            Location {
                path: config.path.join("muru.toml"),
                rule: config.rule,
            }
        } else {
            config
        };

        Self {
            mode,
            config,
            state,
            run,
            logs,
            cache,
        }
    }

    /// The admin socket inside the runtime directory.
    pub fn admin_socket(&self) -> PathBuf {
        self.run.path.join("admin.sock")
    }

    /// The state store inside the state directory.
    pub fn state_store(&self) -> PathBuf {
        self.state.path.join("muru.sqlite")
    }

    /// The daemon's own log file, for a background daemon.
    pub fn daemon_log(&self) -> PathBuf {
        self.logs.path.join("daemon.log")
    }

    /// The daemon's pid file, for a background daemon.
    pub fn pid_file(&self) -> PathBuf {
        self.run.path.join("daemon.pid")
    }
}

/// The first rule that applies: option, environment, systemd, default.
fn pick(
    option: Option<&PathBuf>,
    variable: &str,
    systemd: &str,
    default: impl FnOnce() -> PathBuf,
) -> Location {
    if let Some(path) = option {
        return Location {
            path: path.clone(),
            rule: "option",
        };
    }

    if let Some(path) = env::var_os(variable).filter(|value| !value.is_empty()) {
        return Location {
            path: PathBuf::from(path),
            rule: "environment",
        };
    }

    // systemd lists several directories separated by colons; the first
    // is the unit's own.

    if let Some(value) = env::var_os(systemd).filter(|value| !value.is_empty()) {
        let first = value
            .to_string_lossy()
            .split(':')
            .next()
            .unwrap_or_default()
            .to_owned();
        return Location {
            path: PathBuf::from(first),
            rule: "systemd",
        };
    }

    Location {
        path: default(),
        rule: "default",
    }
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.

    unsafe { libc::geteuid() == 0 }
}

fn home_or_cwd(home: Option<&std::path::Path>) -> PathBuf {
    home.map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn xdg(variable: &str, home: Option<&std::path::Path>, fallback: &str) -> PathBuf {
    env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_or_cwd(home).join(fallback))
}

fn default_config(mode: Mode, home: Option<&std::path::Path>) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/etc/muru/muru.toml"),
        Mode::User if cfg!(target_os = "macos") => {
            home_or_cwd(home).join("Library/Application Support/muru/muru.toml")
        }
        Mode::User => xdg("XDG_CONFIG_HOME", home, ".config").join("muru/muru.toml"),
    }
}

fn default_state(mode: Mode, home: Option<&std::path::Path>) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/var/lib/muru"),
        Mode::User if cfg!(target_os = "macos") => {
            home_or_cwd(home).join("Library/Application Support/muru")
        }
        Mode::User => xdg("XDG_STATE_HOME", home, ".local/state").join("muru"),
    }
}

fn default_run(mode: Mode) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/run/muru"),
        // Short, for the 104-byte limit on socket paths.
        Mode::User if cfg!(target_os = "macos") => {
            // SAFETY: getuid has no preconditions and cannot fail.

            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/muru-{uid}"))
        }
        Mode::User => match env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
            Some(dir) => PathBuf::from(dir).join("muru"),
            None => {
                // SAFETY: getuid has no preconditions and cannot fail.

                let uid = unsafe { libc::getuid() };
                PathBuf::from(format!("/tmp/muru-{uid}"))
            }
        },
    }
}

fn default_logs(mode: Mode, home: Option<&std::path::Path>) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/var/log/muru"),
        Mode::User if cfg!(target_os = "macos") => home_or_cwd(home).join("Library/Logs/muru"),
        Mode::User => xdg("XDG_STATE_HOME", home, ".local/state").join("muru/logs"),
    }
}

fn default_cache(mode: Mode, home: Option<&std::path::Path>) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/var/cache/muru"),
        Mode::User if cfg!(target_os = "macos") => home_or_cwd(home).join("Library/Caches/muru"),
        Mode::User => xdg("XDG_CACHE_HOME", home, ".cache").join("muru"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_win_over_defaults() {
        let options = Options {
            state_dir: Some(PathBuf::from("/tmp/state")),
            run_dir: Some(PathBuf::from("/tmp/run")),
            ..Options::default()
        };
        let locations = Locations::resolve(&options);

        assert_eq!(locations.state.path, PathBuf::from("/tmp/state"));
        assert_eq!(locations.state.rule, "option");
        assert_eq!(
            locations.admin_socket(),
            PathBuf::from("/tmp/run/admin.sock")
        );
        assert_eq!(locations.cache.rule, "default");
    }

    #[test]
    fn system_mode_uses_the_system_paths() {
        let options = Options {
            system: true,
            ..Options::default()
        };
        let locations = Locations::resolve(&options);

        assert_eq!(locations.mode, Mode::System);
        assert_eq!(locations.state.path, PathBuf::from("/var/lib/muru"));
        assert_eq!(locations.run.path, PathBuf::from("/run/muru"));
        assert!(locations.config.path.ends_with("muru.toml"));
    }
}
