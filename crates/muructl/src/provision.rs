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
