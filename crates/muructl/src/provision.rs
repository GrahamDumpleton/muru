//! Provisioning as `muructl run`, `muructl build` and `muructl runtime`
//! see it (design specification section 8): the developer's own uv and
//! directories, the user cache for the runtime library, and the
//! environment variables that stand in for the daemon's `[tools]` table.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Subcommand;

/// The setting that names a wheel directory in development.
pub const WHEEL_DIR_VARIABLE: &str = "MURU_WHEEL_DIR";

/// muructl's version as PyPI spells it.
pub fn version() -> String {
    muru_provision::runtime::pep440(env!("CARGO_PKG_VERSION"))
}

/// Whether the network is off limits: `MURU_OFFLINE`, or uv's own
/// `UV_OFFLINE`, set to anything but an empty string, 0 or false.
pub fn offline() -> bool {
    ["MURU_OFFLINE", "UV_OFFLINE"].iter().any(|name| {
        std::env::var(name)
            .ok()
            .is_some_and(|value| !matches!(value.trim(), "" | "0" | "false" | "no"))
    })
}

/// The wheel directory named by `MURU_WHEEL_DIR`, if any.
pub fn wheel_dir() -> Option<PathBuf> {
    std::env::var_os(WHEEL_DIR_VARIABLE)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Provide the runtime library for a probed interpreter in development:
/// the environment's own `muru` when it matches, else the user cache,
/// else a fetch (section 8.3).
pub async fn runtime(probe: &muru_provision::Probe) -> anyhow::Result<muru_provision::Provided> {
    let root = crate::devcert::cache_dir()?.join("runtime");
    let version = version();
    let wheel_dir = wheel_dir();
    let options = muru_provision::runtime::Options {
        probe,
        version: &version,
        root: &root,
        wheel_dir: wheel_dir.as_deref(),
        offline: offline(),
    };

    Ok(muru_provision::runtime::provide(&options, WHEEL_DIR_VARIABLE).await?)
}

/// `muructl runtime`: the runtime library on its own.
#[derive(Debug, Subcommand)]
pub enum RuntimeCommand {
    /// Unpack the runtime library for an interpreter into a directory to
    /// put on PYTHONPATH, which is how an image gets it.
    Install {
        /// The interpreter the library is for.
        #[arg(long, value_name = "PYTHON")]
        python: PathBuf,

        /// The directory to unpack into; it then holds the muru package.
        #[arg(long, value_name = "DIR")]
        target: PathBuf,

        /// A directory of wheels to look in before PyPI (default:
        /// MURU_WHEEL_DIR).
        #[arg(long, value_name = "DIR")]
        wheel_dir: Option<PathBuf>,

        /// Never use the network (default: MURU_OFFLINE or UV_OFFLINE).
        #[arg(long)]
        offline: bool,
    },
}

/// Run a `muructl runtime` command.
pub async fn run(command: RuntimeCommand) -> anyhow::Result<()> {
    match command {
        RuntimeCommand::Install {
            python,
            target,
            wheel_dir,
            offline: flag,
        } => {
            let probe = muru_provision::probe::probe(&python)
                .with_context(|| format!("probing {}", python.display()))?;

            if let Some(reason) = probe.unsupported() {
                anyhow::bail!("{}: {reason}", python.display());
            }

            let version = version();
            let wheel_dir = wheel_dir.or_else(self::wheel_dir);
            let options = muru_provision::runtime::Options {
                probe: &probe,
                version: &version,
                root: Path::new("."),
                wheel_dir: wheel_dir.as_deref(),
                offline: flag || offline(),
            };
            let filename = muru_provision::runtime::install(&options, &target).await?;
            println!(
                "muru: unpacked {filename} into {} for {}; put the directory first on PYTHONPATH",
                target.display(),
                probe.describe()
            );
            Ok(())
        }
    }
}

/// The build mode in force for a run: the flag, else `muru.local.toml`,
/// else the project's `[tool.muru.build]`, else auto in development mode
/// and never under --allow-public, where nothing is built unasked.
pub fn effective_when(
    flag: Option<&str>,
    local: Option<muru_config::BuildWhen>,
    table: Option<&muru_config::Build>,
    development: bool,
) -> anyhow::Result<muru_config::BuildWhen> {
    if let Some(text) = flag {
        return text
            .parse()
            .map_err(|error: String| anyhow::anyhow!("--build {error}"));
    }

    Ok(local
        .or_else(|| table.and_then(|table| table.when))
        .unwrap_or(if development {
            muru_config::BuildWhen::Auto
        } else {
            muru_config::BuildWhen::Never
        }))
}

/// The `--python` or `python` value as uv's `--python` takes it: a request
/// as it is, a path made the project's.
fn python_for_uv(flag: Option<&str>, table: Option<&str>, project: &Path) -> Option<String> {
    flag.or(table)
        .map(|text| match muru_provision::Python::parse(text, project) {
            muru_provision::Python::Path(path) => path.display().to_string(),
            muru_provision::Python::Request(request) => request,
        })
}

/// Section 8.2: the build `muructl run` does before it serves, under the
/// build mode. Returns whether an environment was built.
pub async fn prepare(
    project: &Path,
    when: muru_config::BuildWhen,
    flag: Option<&str>,
    table: Option<&str>,
    development: bool,
) -> anyhow::Result<bool> {
    use muru_config::BuildWhen;

    let lock = muru_provision::build::lock_file(project);
    let environment = muru_provision::build::environment(project, development);
    let python = python_for_uv(flag, table, project);

    if lock.is_some() && muru_provision::build::lock_behind_manifest(project) {
        println!(
            "muru: pyproject.toml is newer than uv.lock; run uv lock if the dependencies changed"
        );
    }

    match (when, &lock, &environment) {
        (_, None, _) => Ok(false),
        (BuildWhen::Never, Some(_), None) => {
            println!(
                "muru: no environment for this project and the build mode is never; muructl build \
                 makes one from uv.lock"
            );
            Ok(false)
        }
        (BuildWhen::Never, Some(_), Some(_)) => Ok(false),
        (BuildWhen::Auto, Some(_), None) | (BuildWhen::Always, Some(_), _) => {
            let reason = if environment.is_some() {
                "the build mode is always"
            } else {
                "no environment for this project"
            };

            build_environment(project, python.as_deref(), development, reason).await
        }
        (BuildWhen::Auto, Some(_), Some(_)) => {
            // Used as it is, however stale, but said when it is behind.

            let Some(uv) = muru_provision::Uv::find(None, None) else {
                return Ok(false);
            };
            let project = project.to_path_buf();
            let behind = tokio::task::spawn_blocking(move || {
                muru_provision::build::behind_lock(
                    &project,
                    python.as_deref(),
                    &uv,
                    &muru_provision::Isolation::default(),
                )
            })
            .await?;

            match behind {
                Ok(Some(changes)) => println!(
                    "muru: the environment is behind uv.lock ({changes}); run with --build, or \
                     muructl build, to bring it up to date"
                ),
                Ok(None) => {}
                Err(error) => {
                    println!("muru: could not compare the environment with uv.lock: {error}")
                }
            }

            Ok(false)
        }
    }
}

/// Section 8.2 under reload: a change to the lock or to what names the
/// interpreter builds before the new release; a manifest change alone
/// does not. `requires` is the requires-python last seen, updated here.
/// Returns whether an environment was built.
pub async fn rebuild(
    project: &Path,
    when: muru_config::BuildWhen,
    flag: Option<&str>,
    table: Option<&str>,
    development: bool,
    changed: &[PathBuf],
    requires: &mut Option<String>,
) -> anyhow::Result<bool> {
    let names: Vec<&str> = changed
        .iter()
        .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
        .collect();
    let lock_changed = names.contains(&muru_provision::build::LOCK_FILE);
    let pin_changed = names.contains(&".python-version");
    let manifest_changed = names.contains(&"pyproject.toml");
    let now = muru_provision::interpreter::requires_python(project).map(|set| set.to_string());
    let requirement_changed = manifest_changed && now != *requires;
    *requires = now;

    if when == muru_config::BuildWhen::Never || muru_provision::build::lock_file(project).is_none()
    {
        return Ok(false);
    }

    if lock_changed || pin_changed || requirement_changed {
        let reason = if lock_changed {
            "uv.lock changed"
        } else if pin_changed {
            ".python-version changed"
        } else {
            "requires-python changed"
        };
        let python = python_for_uv(flag, table, project);
        return build_environment(project, python.as_deref(), development, reason).await;
    }

    if manifest_changed && muru_provision::build::lock_behind_manifest(project) {
        println!(
            "muru: pyproject.toml changed and is newer than uv.lock; run uv lock if the \
             dependencies changed"
        );
    }

    Ok(false)
}

/// Build the project's environment in the development flavour, with
/// the developer's own uv, streaming uv's lines to the terminal. Without
/// uv nothing is built and the message says what to install.
async fn build_environment(
    project: &Path,
    python: Option<&str>,
    development: bool,
    reason: &str,
) -> anyhow::Result<bool> {
    let Some(uv) = muru_provision::Uv::find(None, None) else {
        println!(
            "muru: {reason}, but uv was not found on PATH, beside muructl or in MURU_UV, so the \
             environment was not built; install uv, or make the environment yourself"
        );
        return Ok(false);
    };
    let groups: Vec<String> = Vec::new();
    let build = muru_provision::Build {
        project,
        python,
        deployment: false,
        deps_only: false,
        target: None,
        extras: &groups,
        groups: &groups,
        source_builds: false,
        compile_bytecode: false,
    };
    println!("muru: {reason}, building .venv with {}", build.describe());
    let isolation = muru_provision::Isolation::default();
    let owned = OwnedBuild::from(&build);
    let _ = development;

    tokio::task::spawn_blocking(move || {
        owned
            .as_build()
            .run(&uv, &isolation, &mut |line| println!("muru: {line}"))
    })
    .await?
    .map_err(|error| anyhow::anyhow!("the environment build failed: {error}"))?;

    Ok(true)
}

/// A build with its own storage, for a blocking task.
struct OwnedBuild {
    project: PathBuf,
    python: Option<String>,
    deployment: bool,
    deps_only: bool,
    target: Option<PathBuf>,
    extras: Vec<String>,
    groups: Vec<String>,
    source_builds: bool,
    compile_bytecode: bool,
}

impl From<&muru_provision::Build<'_>> for OwnedBuild {
    fn from(build: &muru_provision::Build<'_>) -> Self {
        Self {
            project: build.project.to_path_buf(),
            python: build.python.map(str::to_owned),
            deployment: build.deployment,
            deps_only: build.deps_only,
            target: build.target.map(Path::to_path_buf),
            extras: build.extras.to_vec(),
            groups: build.groups.to_vec(),
            source_builds: build.source_builds,
            compile_bytecode: build.compile_bytecode,
        }
    }
}

impl OwnedBuild {
    fn as_build(&self) -> muru_provision::Build<'_> {
        muru_provision::Build {
            project: &self.project,
            python: self.python.as_deref(),
            deployment: self.deployment,
            deps_only: self.deps_only,
            target: self.target.as_deref(),
            extras: &self.extras,
            groups: &self.groups,
            source_builds: self.source_builds,
            compile_bytecode: self.compile_bytecode,
        }
    }
}

/// `muructl build`: the provisioning procedure as a command.
#[derive(Debug, clap::Args)]
pub struct BuildArgs {
    /// The project directory (default: the current directory).
    #[arg(value_name = "PATH")]
    project: Option<PathBuf>,

    /// Build what the daemon or an image runs: no dev group, no source
    /// builds, bytecode compiled, the project's extras and groups.
    #[arg(long)]
    deployment: bool,

    /// Dependencies only, before the source is copied in.
    #[arg(long, requires = "deployment")]
    deps_only: bool,

    /// Where the environment goes (default: the project's .venv).
    #[arg(long, value_name = "DIR")]
    target: Option<PathBuf>,

    /// Where the runtime library goes (default: the user cache).
    #[arg(long, value_name = "DIR")]
    runtime_dir: Option<PathBuf>,

    /// The interpreter: a path, or a version request such as 3.13.
    #[arg(long, value_name = "PATH or VERSION")]
    python: Option<String>,

    /// An extra to install, deployment flavour; repeatable.
    #[arg(long, value_name = "NAME")]
    extra: Vec<String>,

    /// A dependency group to install, deployment flavour; repeatable.
    #[arg(long, value_name = "NAME")]
    group: Vec<String>,

    /// Skip the runtime library, for an image that brings its own.
    #[arg(long)]
    no_runtime: bool,
}

/// Run `muructl build`.
pub async fn build(args: BuildArgs) -> anyhow::Result<()> {
    let project = args
        .project
        .unwrap_or_else(|| PathBuf::from("."))
        .canonicalize()
        .context("the project directory does not exist")?;
    let table = muru_config::load_project(&project)?
        .map(|(_, table)| table)
        .unwrap_or_default();
    let settings = table.build.clone().unwrap_or_default();

    if muru_provision::build::lock_file(&project).is_none() {
        anyhow::bail!(
            "{} has no uv.lock; Muru installs from a lock and never resolves, so run uv lock and \
             commit the file",
            project.display()
        );
    }

    let Some(uv) = muru_provision::Uv::find(None, None) else {
        anyhow::bail!(
            "uv was not found on PATH, beside muructl or in MURU_UV, and building needs it; \
             install uv, or set MURU_UV to where it is"
        );
    };
    let python = python_for_uv(args.python.as_deref(), table.python.as_deref(), &project);
    let mut extras = settings.extras.clone();
    extras.extend(args.extra.iter().cloned());
    let mut groups = settings.groups.clone();
    groups.extend(args.group.iter().cloned());
    let build = muru_provision::Build {
        project: &project,
        python: python.as_deref(),
        deployment: args.deployment,
        deps_only: args.deps_only,
        target: args.target.as_deref(),
        extras: &extras,
        groups: &groups,
        source_builds: settings.source_builds.unwrap_or(false),
        compile_bytecode: settings.compile_bytecode.unwrap_or(true),
    };
    let environment = args.target.clone().unwrap_or_else(|| project.join(".venv"));
    println!(
        "muru: building {} with {}",
        environment.display(),
        build.describe()
    );
    let owned = OwnedBuild::from(&build);
    let isolation = muru_provision::Isolation::default();

    tokio::task::spawn_blocking(move || {
        owned
            .as_build()
            .run(&uv, &isolation, &mut |line| println!("muru: {line}"))
    })
    .await?
    .map_err(|error| anyhow::anyhow!("the environment build failed: {error}"))?;

    // What was built, probed, and the runtime library for it.

    let interpreter = environment.join("bin").join("python");
    let probe = muru_provision::probe::probe(&interpreter)
        .with_context(|| format!("probing {}", interpreter.display()))?;

    if let Some(reason) = probe.unsupported() {
        anyhow::bail!("{}: {reason}", interpreter.display());
    }

    println!(
        "muru: built {} on {}",
        environment.display(),
        probe.describe()
    );

    if args.no_runtime {
        return Ok(());
    }

    match &args.runtime_dir {
        Some(dir) => {
            let version = version();
            let wheel_dir = wheel_dir();
            let options = muru_provision::runtime::Options {
                probe: &probe,
                version: &version,
                root: Path::new("."),
                wheel_dir: wheel_dir.as_deref(),
                offline: offline(),
            };
            let filename = muru_provision::runtime::install(&options, dir).await?;
            println!(
                "muru: unpacked the runtime library {filename} into {}",
                dir.display()
            );
        }
        None => {
            let library = runtime(&probe).await?;

            for note in &library.notes {
                println!("muru: {note}");
            }

            println!("muru: {}", library.describe());
        }
    }

    Ok(())
}
