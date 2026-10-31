//! uv as muructl runs it (design specification section 8.4, ADR 0007): a
//! separate program found through a lookup order, run with the
//! isolation the mode asks for, never on the request path.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::specifier::Version;

/// The name of the environment variable that names uv.
pub const UV_VARIABLE: &str = "MURU_UV";

/// How uv was found, for the line that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// `tools.uv` in the daemon's configuration.
    Configured,
    /// `MURU_UV` in the environment.
    Variable,
    /// A `uv` beside the muructl executable, which the `muructl[uv]`
    /// extra provides.
    Beside,
    /// `uv` on PATH.
    OnPath,
    /// The managed copy under the tools directory.
    Managed,
}

impl Found {
    /// How the lookup is described.
    pub fn as_str(self) -> &'static str {
        match self {
            Found::Configured => "tools.uv",
            Found::Variable => UV_VARIABLE,
            Found::Beside => "beside muructl",
            Found::OnPath => "PATH",
            Found::Managed => "the managed copy",
        }
    }
}

/// A uv executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uv {
    /// Where it is.
    pub path: PathBuf,
    /// How it was found.
    pub found: Found,
}

/// What muructl sets around uv (section 8.4). Development mode sets
/// nothing, so the developer's own uv, Pythons and configuration apply;
/// the daemon isolates its directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Isolation {
    /// `UV_PYTHON_INSTALL_DIR`: where managed interpreters go.
    pub python_install_dir: Option<PathBuf>,
    /// `UV_CACHE_DIR`.
    pub cache_dir: Option<PathBuf>,
    /// `UV_PYTHON_DOWNLOADS=never`.
    pub no_downloads: bool,
    /// `UV_OFFLINE=1`.
    pub offline: bool,
    /// `UV_NO_CONFIG=1`, for the interpreter operations where no project
    /// settings matter.
    pub no_config: bool,
    /// `UV_CONFIG_FILE`: the daemon's own uv.toml for builds, over which
    /// the project's `[tool.uv]` still applies.
    pub config_file: Option<PathBuf>,
}

/// What went wrong running uv.
#[derive(Debug, thiserror::Error)]
pub enum UvError {
    /// uv could not be started.
    #[error("{uv} could not be run: {source}")]
    Spawn {
        /// The executable.
        uv: PathBuf,
        /// The error.
        source: std::io::Error,
    },
    /// uv exited with an error, quoted.
    #[error("{command} failed: {message}")]
    Failed {
        /// The command, as `uv python find 3.13`.
        command: String,
        /// Its last line of output.
        message: String,
    },
}

impl Uv {
    /// Find uv by the lookup order of section 8.4: `configured` (the
    /// daemon's `tools.uv`), `MURU_UV`, a uv beside this executable, uv on
    /// PATH, then the managed copy in `tools_dir`.
    pub fn find(configured: Option<&Path>, tools_dir: Option<&Path>) -> Option<Self> {
        if let Some(path) = configured {
            return Some(Self {
                path: path.to_path_buf(),
                found: Found::Configured,
            });
        }

        if let Some(path) = std::env::var_os(UV_VARIABLE).filter(|value| !value.is_empty()) {
            return Some(Self {
                path: PathBuf::from(path),
                found: Found::Variable,
            });
        }

        let beside = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("uv")))
            .filter(|path| path.is_file());

        if let Some(path) = beside {
            return Some(Self {
                path,
                found: Found::Beside,
            });
        }

        if let Some(path) = find_on_path("uv") {
            return Some(Self {
                path,
                found: Found::OnPath,
            });
        }

        tools_dir
            .map(|dir| dir.join("uv"))
            .filter(|path| path.is_file())
            .map(|path| Self {
                path,
                found: Found::Managed,
            })
    }

    /// `uv --version`, as a version.
    pub fn version(&self) -> Result<Version, UvError> {
        let output = self.run(&["--version"], &Isolation::default())?;

        Version::parse(output.split_whitespace().nth(1).unwrap_or_default()).ok_or_else(|| {
            UvError::Failed {
                command: format!("{} --version", self.path.display()),
                message: format!("unexpected output {output:?}"),
            }
        })
    }

    /// `uv python find REQUEST`: the interpreter matching a request, or
    /// None when the machine has none. Never downloads.
    pub fn python_find(
        &self,
        request: &str,
        isolation: &Isolation,
    ) -> Result<Option<PathBuf>, UvError> {
        let mut command = self.command(isolation);
        command.args(["python", "find", request, "--no-project"]);
        command.env("UV_PYTHON_DOWNLOADS", "never");
        let output = command.output().map_err(|source| UvError::Spawn {
            uv: self.path.clone(),
            source,
        })?;
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            return Ok((!path.is_empty()).then(|| PathBuf::from(path)));
        }

        if stderr.contains("No interpreter found") {
            return Ok(None);
        }

        Err(UvError::Failed {
            command: format!("uv python find {request}"),
            message: last_line(&stderr),
        })
    }

    /// `uv python install REQUEST`, into the isolation's install directory.
    /// Its output is returned for the log.
    pub fn python_install(&self, request: &str, isolation: &Isolation) -> Result<String, UvError> {
        self.run(&["python", "install", request], isolation)
    }

    /// A command for uv with the isolation applied, quiet and uncoloured
    /// because its output goes to a log or a prefixed terminal.
    pub fn command(&self, isolation: &Isolation) -> Command {
        let mut command = Command::new(&self.path);
        command
            .env("UV_NO_PROGRESS", "1")
            .arg("--color")
            .arg("never");

        if let Some(dir) = &isolation.python_install_dir {
            command.env("UV_PYTHON_INSTALL_DIR", dir);
        }

        if let Some(dir) = &isolation.cache_dir {
            command.env("UV_CACHE_DIR", dir);
        }

        if isolation.no_downloads {
            command.env("UV_PYTHON_DOWNLOADS", "never");
        }

        if isolation.offline {
            command.env("UV_OFFLINE", "1");
        }

        if isolation.no_config {
            command.env("UV_NO_CONFIG", "1");
        }

        if let Some(file) = &isolation.config_file {
            command.env("UV_CONFIG_FILE", file);
        }

        command
    }

    fn run(&self, args: &[&str], isolation: &Isolation) -> Result<String, UvError> {
        let mut command = self.command(isolation);
        command.args(args);
        let output = command.output().map_err(|source| UvError::Spawn {
            uv: self.path.clone(),
            source,
        })?;

        if !output.status.success() {
            return Err(UvError::Failed {
                command: format!("uv {}", args.join(" ")),
                message: last_line(&String::from_utf8_lossy(&output.stderr)),
            });
        }

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(text)
    }
}

/// The last non-empty line of a program's output, which is where uv and
/// pip put the reason.
fn last_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_owned()
}

/// Find an executable on PATH.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake uv: a script that answers `python find` with `answer` (a
    /// path, or nothing with uv's own error) and `--version` with 0.9.9.
    pub(crate) fn fake_uv(dir: &Path, answer: Option<&str>) -> PathBuf {
        let path = dir.join("uv");

        let find = match answer {
            Some(answer) => format!("echo '{answer}'"),
            None => "echo 'error: No interpreter found for Python 3.99 in virtual environments, managed installations, or search path' >&2; exit 2".to_owned(),
        };

        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/calls\"\ncase \"$*\" in\n*--version*) echo 'uv 0.9.9';;\n*python\\ find*) {find};;\n*) echo \"unexpected $*\" >&2; exit 1;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_configured_path_wins_and_version_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake_uv(dir.path(), Some("/opt/python/bin/python3.13"));
        let uv = Uv::find(Some(&fake), None).unwrap();

        assert_eq!(uv.found, Found::Configured);
        assert_eq!(uv.version().unwrap(), Version::parse("0.9.9").unwrap());
        assert_eq!(
            uv.python_find("3.13", &Isolation::default()).unwrap(),
            Some(PathBuf::from("/opt/python/bin/python3.13"))
        );
    }

    #[test]
    fn a_request_the_machine_cannot_satisfy_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake_uv(dir.path(), None);
        let uv = Uv::find(Some(&fake), None).unwrap();

        assert_eq!(uv.python_find("3.99", &Isolation::default()).unwrap(), None);
        assert!(
            std::fs::read_to_string(dir.path().join("calls"))
                .unwrap()
                .contains("python find 3.99 --no-project")
        );
    }

    #[test]
    fn the_managed_copy_is_the_last_resort() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        fake_uv(&tools, None);

        // Nothing on PATH and nothing beside the test executable, so the
        // managed copy is what is found.

        let found = {
            let _guard = EnvGuard::set("PATH", dir.path().join("empty"));
            Uv::find(None, Some(&tools))
        };

        assert_eq!(found.map(|uv| uv.found), Some(Found::Managed));
    }

    /// Set a variable for the test and restore it after.
    pub(crate) struct EnvGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        pub(crate) fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(name);

            // SAFETY: tests run single-threaded for the environment they
            // touch (cargo nextest runs each test in its own process).

            unsafe { std::env::set_var(name, value) };
            Self { name, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: as above, restoring what the guard replaced.

            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.name, value),
                    None => std::env::remove_var(self.name),
                }
            }
        }
    }
}
