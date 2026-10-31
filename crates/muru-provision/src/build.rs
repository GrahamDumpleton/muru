//! Environment builds (design specification section 8.2): `uv sync
//! --frozen` in the development flavour or the deployment flavour, run
//! for `muructl build`, `muructl run` and the daemon's Building state
//! alike, with the output streamed to whoever asked.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::uv::{Isolation, Uv, UvError};

/// The lock a build installs from.
pub const LOCK_FILE: &str = "uv.lock";

/// The files a change to which means a new release needs a build, or at
/// least a look: the lock, the manifest and uv's pin.
pub const TRIGGER_FILES: [&str; 3] = [LOCK_FILE, "pyproject.toml", ".python-version"];

/// One build.
#[derive(Debug, Clone)]
pub struct Build {
    /// The project directory.
    pub project: PathBuf,
    /// `--python`, a path or request, when one was given.
    pub python: Option<String>,
    /// The deployment flavour: no dev group, no source builds, bytecode
    /// compiled, the project non-editable outside the source.
    pub deployment: bool,
    /// Dependencies only, before the source is copied in (an image's
    /// dependency layer).
    pub deps_only: bool,
    /// Where the environment goes; the project's `.venv` when none.
    pub target: Option<PathBuf>,
    /// Extras to install, deployment flavour.
    pub extras: Vec<String>,
    /// Dependency groups beyond the defaults, deployment flavour.
    pub groups: Vec<String>,
    /// Whether sdists may be built, deployment flavour.
    pub source_builds: bool,
    /// Whether bytecode is compiled, deployment flavour.
    pub compile_bytecode: bool,
}

/// Why a build failed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// uv could not be run.
    #[error(transparent)]
    Uv(#[from] UvError),
    /// uv exited with an error; its output was streamed already.
    #[error("uv sync failed: {last}")]
    Failed {
        /// The last line uv printed.
        last: String,
    },
    /// There is no lock to install from.
    #[error(
        "{project} has no {LOCK_FILE}; Muru installs from a lock and never resolves, so run `uv lock` and commit the file"
    )]
    NoLock {
        /// The project.
        project: PathBuf,
    },
}

impl Build {
    /// The arguments after `uv`.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["sync".to_owned(), "--frozen".to_owned()];
        args.push("--project".to_owned());
        args.push(self.project.display().to_string());

        if let Some(python) = &self.python {
            args.push("--python".to_owned());
            args.push(python.clone());
        }

        if self.deployment {
            args.push("--no-dev".to_owned());

            if !self.source_builds {
                args.push("--no-build".to_owned());
            }

            if self.compile_bytecode {
                args.push("--compile-bytecode".to_owned());
            }

            if self.deps_only {
                args.push("--no-install-project".to_owned());
            }

            if self
                .target
                .as_ref()
                .is_some_and(|target| !target.starts_with(&self.project))
            {
                args.push("--no-editable".to_owned());
            }

            for extra in &self.extras {
                args.push("--extra".to_owned());
                args.push(extra.clone());
            }

            for group in &self.groups {
                args.push("--group".to_owned());
                args.push(group.clone());
            }
        }

        args
    }

    /// The command as a user would type it, for the line that announces
    /// it: the project and its path left out, since they are where the
    /// user is.
    pub fn describe(&self) -> String {
        let args = self.args();
        let mut shown = Vec::new();
        let mut skip = false;

        for arg in &args {
            if skip {
                skip = false;
                continue;
            }

            if arg == "--project" {
                skip = true;
                continue;
            }

            shown.push(arg.as_str());
        }

        format!("uv {}", shown.join(" "))
    }

    /// The uv command for the build, for a caller that changes how it runs
    /// (as another user, say) before `run_command`.
    pub fn command(&self, uv: &Uv, isolation: &Isolation) -> std::process::Command {
        let mut command = uv.command(isolation);
        command.args(self.args());

        if let Some(target) = &self.target {
            command.env("UV_PROJECT_ENVIRONMENT", target);
        }

        command
    }

    /// Run the build with `uv`, streaming each line of its output to
    /// `on_line` as it arrives.
    pub fn run(
        &self,
        uv: &Uv,
        isolation: &Isolation,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), BuildError> {
        self.run_command(self.command(uv, isolation), on_line)
    }

    /// Run a command made by `command`, streaming each line of its output
    /// to `on_line` as it arrives.
    pub fn run_command(
        &self,
        mut command: std::process::Command,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), BuildError> {
        if !self.project.join(LOCK_FILE).is_file() {
            return Err(BuildError::NoLock {
                project: self.project.clone(),
            });
        }

        let uv = PathBuf::from(command.get_program());
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|source| UvError::Spawn {
            uv: uv.clone(),
            source,
        })?;

        // uv writes its progress to stderr and little to stdout; both are
        // read on threads of their own and merged in arrival order.

        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        let readers: Vec<_> = [
            child
                .stdout
                .take()
                .map(|out| Box::new(out) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|err| Box::new(err) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        .map(|stream| {
            let sender = sender.clone();

            std::thread::spawn(move || {
                for line in std::io::BufReader::new(stream).lines() {
                    let Ok(line) = line else {
                        break;
                    };

                    if sender.send(line).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();
        drop(sender);
        let mut last = String::new();

        for line in receiver {
            let line = line.trim_end();

            if line.is_empty() {
                continue;
            }

            on_line(line);
            last = line.to_owned();
        }

        for reader in readers {
            let _ = reader.join();
        }

        let status = child
            .wait()
            .map_err(|source| UvError::Spawn { uv, source })?;

        if !status.success() {
            return Err(BuildError::Failed {
                last: if last.is_empty() {
                    format!("exit status {status}")
                } else {
                    last
                },
            });
        }

        Ok(())
    }
}

/// The lock's SHA-256, shortened, for the record of what was built.
pub fn lock_digest(project: &Path) -> Option<String> {
    let bytes = std::fs::read(project.join(LOCK_FILE)).ok()?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let hex: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(hex[..12].to_owned())
}

/// The project's lock, if it has one.
pub fn lock_file(project: &Path) -> Option<PathBuf> {
    let path = project.join(LOCK_FILE);
    path.is_file().then_some(path)
}

/// Whether the manifest was written after the lock, which is how the two
/// drift: a `pyproject.toml` edited by hand without `uv lock`.
pub fn lock_behind_manifest(project: &Path) -> bool {
    let modified = |name: &str| {
        std::fs::metadata(project.join(name))
            .and_then(|metadata| metadata.modified())
            .ok()
    };

    match (modified(LOCK_FILE), modified("pyproject.toml")) {
        (Some(lock), Some(manifest)) => manifest > lock,
        _ => false,
    }
}

/// The environment a run would use without building, if one exists: the
/// active virtualenv in development, else the project's `.venv`.
pub fn environment(project: &Path, development: bool) -> Option<PathBuf> {
    if development
        && let Some(venv) = std::env::var_os("VIRTUAL_ENV").filter(|value| !value.is_empty())
    {
        let python = PathBuf::from(venv).join("bin").join("python");

        if python.is_file() {
            return Some(python);
        }
    }

    let python = project.join(".venv").join("bin").join("python");
    python.is_file().then_some(python)
}

/// What `uv sync --frozen --dry-run` would change in the project's
/// environment, as one line, or None when it is up to date with the lock.
pub fn behind_lock(
    project: &Path,
    python: Option<&str>,
    uv: &Uv,
    isolation: &Isolation,
) -> Result<Option<String>, UvError> {
    let mut command = uv.command(isolation);
    command.args(["sync", "--frozen", "--dry-run", "--project"]);
    command.arg(project);

    if let Some(python) = python {
        command.arg("--python").arg(python);
    }

    let output = command.output().map_err(|source| UvError::Spawn {
        uv: uv.path.clone(),
        source,
    })?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    if !output.status.success() {
        return Err(UvError::Failed {
            command: "uv sync --frozen --dry-run".to_owned(),
            message: text
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .unwrap_or("no output")
                .to_owned(),
        });
    }

    if text.contains("Would make no changes") {
        return Ok(None);
    }

    // The summary lines: "Would install 3 packages", "Would uninstall 1
    // package", each a change the environment is behind by.

    let changes: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("Would ")
                && !line.starts_with("Would use")
                && !line.starts_with("Would create")
        })
        .collect();

    Ok(Some(if changes.is_empty() {
        "the environment differs from the lock".to_owned()
    } else {
        changes.join(", ").to_lowercase()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_flavours_spell_their_uv_commands() {
        let development = Build {
            project: PathBuf::from("/srv/shop"),
            python: Some("3.13".to_owned()),
            deployment: false,
            deps_only: false,
            target: None,
            extras: vec!["postgres".to_owned()],
            groups: Vec::new(),
            source_builds: false,
            compile_bytecode: true,
        };

        assert_eq!(
            development.describe(),
            "uv sync --frozen --python 3.13",
            "the development flavour is what the developer would type"
        );

        let deployment = Build {
            deployment: true,
            deps_only: true,
            target: Some(PathBuf::from("/var/lib/muru/envs/shop/r42")),
            python: None,
            ..development.clone()
        };
        assert_eq!(
            deployment.describe(),
            "uv sync --frozen --no-dev --no-build --compile-bytecode --no-install-project --no-editable --extra postgres"
        );

        let inside = Build {
            target: Some(PathBuf::from("/srv/shop/.venv")),
            source_builds: true,
            deps_only: false,
            ..deployment
        };
        assert_eq!(
            inside.describe(),
            "uv sync --frozen --no-dev --compile-bytecode --extra postgres",
            "an environment inside the source keeps the project editable"
        );
    }

    #[test]
    fn a_project_without_a_lock_cannot_be_built() {
        let dir = tempfile::tempdir().unwrap();
        let fake = crate::uv::tests::fake_uv(dir.path(), None);
        let uv = Uv::find(Some(&fake), None).unwrap();
        let build = Build {
            project: dir.path().to_path_buf(),
            python: None,
            deployment: false,
            deps_only: false,
            target: None,
            extras: Vec::new(),
            groups: Vec::new(),
            source_builds: false,
            compile_bytecode: false,
        };
        let error = build
            .run(&uv, &Isolation::default(), &mut |_| {})
            .unwrap_err();

        assert!(matches!(error, BuildError::NoLock { .. }));
        assert!(error.to_string().contains("run `uv lock`"));
        assert_eq!(lock_file(dir.path()), None);
        assert!(!lock_behind_manifest(dir.path()));
    }

    #[test]
    fn a_manifest_newer_than_the_lock_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LOCK_FILE), "version = 1\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::open(dir.path().join(LOCK_FILE))
            .unwrap()
            .set_modified(old)
            .unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "[project]\nname = 'x'\n").unwrap();

        assert!(lock_behind_manifest(dir.path()));
        assert!(lock_file(dir.path()).is_some());
        assert_eq!(lock_digest(dir.path()).unwrap().len(), 12);
    }
}
