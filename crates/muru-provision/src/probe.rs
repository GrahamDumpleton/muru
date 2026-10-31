//! Probing an interpreter (design specification section 8.1): one run of
//! a short script that says what it is, cached by path and modification
//! time, so the host is never started on a Python that cannot run it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::specifier::Version;

/// The lowest Python Muru supports.
pub const MINIMUM: (u64, u64) = (3, 11);

/// The script the interpreter runs. `-I` keeps the caller's environment
/// out of it while the interpreter's own site-packages stay on the path,
/// so the `muru` it reports is the one the environment holds.
const SCRIPT: &str = r#"
import json, platform, sys
try:
    from importlib.metadata import version
    muru = {"version": version("muru")}
except Exception:
    muru = None
if muru is not None:
    try:
        import importlib.util
        spec = importlib.util.find_spec("muru")
        muru["path"] = spec.submodule_search_locations[0] if spec and spec.submodule_search_locations else ""
    except Exception:
        muru["path"] = ""
print(json.dumps({
    "version": ".".join(str(part) for part in sys.version_info[:3]),
    "implementation": sys.implementation.name,
    "cache_tag": sys.implementation.cache_tag or "",
    "free_threaded": not getattr(sys, "_is_gil_enabled", lambda: True)(),
    "platform": sys.platform,
    "machine": platform.machine(),
    "libc": platform.libc_ver()[0],
    "libc_version": platform.libc_ver()[1],
    "mac_version": platform.mac_ver()[0],
    "prefix": sys.prefix,
    "base_prefix": sys.base_prefix,
    "executable": sys.executable,
    "muru": muru,
}))
"#;

/// What a probe found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Probe {
    /// The version, as `3.13.9`.
    pub version: String,
    /// `cpython` or another implementation.
    pub implementation: String,
    /// `sys.implementation.cache_tag`, as `cpython-313`.
    pub cache_tag: String,
    /// Whether the GIL is disabled.
    pub free_threaded: bool,
    /// `sys.platform`.
    pub platform: String,
    /// The machine, as `arm64` or `x86_64`.
    pub machine: String,
    /// The C library, `glibc` or `musl`, or empty where it is not reported.
    pub libc: String,
    /// The C library's version, as `2.35`, or empty.
    #[serde(default)]
    pub libc_version: String,
    /// The macOS version, as `15.1`, or empty elsewhere.
    #[serde(default)]
    pub mac_version: String,
    /// `sys.prefix`, the virtualenv when there is one.
    pub prefix: PathBuf,
    /// `sys.base_prefix`, the interpreter a virtualenv was made from.
    pub base_prefix: PathBuf,
    /// The executable as the interpreter sees itself.
    pub executable: PathBuf,
    /// The `muru` the interpreter imports, if any.
    pub muru: Option<MuruImport>,
}

/// The runtime library an interpreter has installed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct MuruImport {
    /// Its version.
    pub version: String,
    /// The package directory, or empty when it could not be found.
    #[serde(default)]
    pub path: String,
}

/// Why an interpreter could not be probed.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// It could not be started.
    #[error("{python} could not be run: {source}")]
    Spawn {
        /// The interpreter.
        python: PathBuf,
        /// The error.
        source: std::io::Error,
    },
    /// It ran and failed.
    #[error("{python} failed when asked what it is: {message}")]
    Failed {
        /// The interpreter.
        python: PathBuf,
        /// Its last line of output.
        message: String,
    },
}

impl Probe {
    /// The version as a `Version`.
    pub fn parsed_version(&self) -> Version {
        Version::parse(&self.version).unwrap_or_else(|| Version::parse("0").expect("a version"))
    }

    /// The wheel tag, as `cp313` or `cp314t`.
    pub fn tag(&self) -> String {
        let version = self.parsed_version();
        let suffix = if self.free_threaded { "t" } else { "" };
        format!("cp{}{}{}", version.major(), version.minor(), suffix)
    }

    /// Whether the interpreter runs in a virtualenv.
    pub fn in_virtualenv(&self) -> bool {
        self.prefix != self.base_prefix
    }

    /// Why Muru cannot run on this interpreter, if it cannot.
    pub fn unsupported(&self) -> Option<String> {
        if self.implementation != "cpython" {
            return Some(format!(
                "{} is not supported; Muru runs on CPython {}.{} and later",
                self.implementation, MINIMUM.0, MINIMUM.1
            ));
        }

        let version = self.parsed_version();

        if (version.major(), version.minor()) < MINIMUM {
            return Some(format!(
                "Python {} is not supported; Muru runs on CPython {}.{} and later",
                self.version, MINIMUM.0, MINIMUM.1
            ));
        }

        None
    }

    /// A short description, as `Python 3.13.9 (cp313, free-threaded)`.
    pub fn describe(&self) -> String {
        let mut text = format!("Python {} ({}", self.version, self.tag());

        if self.free_threaded {
            text.push_str(", free-threaded");
        }

        text.push(')');
        text
    }
}

/// Probes by interpreter path and modification time.
type Cache = HashMap<(PathBuf, Option<SystemTime>), Probe>;

/// Probe `python`, from the cache when it has not changed since.
pub fn probe(python: &Path) -> Result<Probe, ProbeError> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let modified = std::fs::metadata(python)
        .and_then(|metadata| metadata.modified())
        .ok();
    let key = (python.to_path_buf(), modified);
    let cache = CACHE.get_or_init(Mutex::default);

    if let Some(found) = cache.lock().expect("probe cache poisoned").get(&key) {
        return Ok(found.clone());
    }

    let found = run(python)?;
    cache
        .lock()
        .expect("probe cache poisoned")
        .insert(key, found.clone());
    Ok(found)
}

fn run(python: &Path) -> Result<Probe, ProbeError> {
    let output = Command::new(python)
        .arg("-I")
        .arg("-c")
        .arg(SCRIPT)
        .output()
        .map_err(|source| ProbeError::Spawn {
            python: python.to_path_buf(),
            source,
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or("no output")
            .to_owned();

        return Err(ProbeError::Failed {
            python: python.to_path_buf(),
            message,
        });
    }

    serde_json::from_str(stdout.trim()).map_err(|error| ProbeError::Failed {
        python: python.to_path_buf(),
        message: format!("unexpected output ({error})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_running_python_is_probed() {
        let Some(python) = crate::uv::find_on_path("python3") else {
            return;
        };
        let found = probe(&python).unwrap();

        assert_eq!(found.implementation, "cpython");
        assert!(found.cache_tag.starts_with("cpython-"));
        assert!(found.tag().starts_with("cp3"));
        assert!(found.executable.is_absolute());
        assert_eq!(probe(&python).unwrap(), found);
    }

    #[test]
    fn unsupported_interpreters_are_named() {
        let mut found = Probe {
            version: "3.9.6".to_owned(),
            implementation: "cpython".to_owned(),
            cache_tag: "cpython-39".to_owned(),
            free_threaded: false,
            platform: "darwin".to_owned(),
            machine: "arm64".to_owned(),
            libc: String::new(),
            libc_version: String::new(),
            mac_version: String::new(),
            prefix: PathBuf::from("/usr"),
            base_prefix: PathBuf::from("/usr"),
            executable: PathBuf::from("/usr/bin/python3"),
            muru: None,
        };

        assert_eq!(
            found.unsupported().unwrap(),
            "Python 3.9.6 is not supported; Muru runs on CPython 3.11 and later"
        );
        assert!(!found.in_virtualenv());

        found.version = "3.14.0".to_owned();
        found.free_threaded = true;
        assert_eq!(found.unsupported(), None);
        assert_eq!(found.tag(), "cp314t");
        assert_eq!(found.describe(), "Python 3.14.0 (cp314t, free-threaded)");

        found.implementation = "pypy".to_owned();
        assert!(
            found
                .unsupported()
                .unwrap()
                .starts_with("pypy is not supported")
        );
    }

    #[test]
    fn a_missing_interpreter_is_a_spawn_error() {
        let error = probe(Path::new("/nonexistent/python")).unwrap_err();
        assert!(matches!(error, ProbeError::Spawn { .. }));
        assert!(
            error
                .to_string()
                .starts_with("/nonexistent/python could not be run")
        );
    }
}
