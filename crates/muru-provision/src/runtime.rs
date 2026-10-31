//! The runtime library provided by muructl (design specification section
//! 8.3, ADR 0037): the `muru` package matching muructl's own version, put
//! on the host's PYTHONPATH for the interpreter in use. Use what exists,
//! and go to the network last.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::probe::Probe;
use crate::specifier::Version;

/// The distribution on PyPI.
const DISTRIBUTION: &str = "muru";

/// How long a download may take in all.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the connection has to be made, so a machine with no route
/// fails in seconds, not minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// muructl's own version as PEP 440 spells it: Cargo's `0.1.0-alpha.12`
/// is PyPI's `0.1.0a12`, which is what maturin writes into the wheel.
pub fn pep440(cargo: &str) -> String {
    let Some((release, pre)) = cargo.split_once('-') else {
        return cargo.to_owned();
    };
    let (kind, number) = pre.split_once('.').unwrap_or((pre, "0"));

    let kind = match kind {
        "alpha" | "a" => "a",
        "beta" | "b" => "b",
        "rc" | "c" | "pre" | "preview" => "rc",
        "dev" => ".dev",
        other => other,
    };

    format!("{release}{kind}{number}")
}

/// The tags of a wheel file name: `muru-0.1.0a12-cp313-cp313t-manylinux_2_28_x86_64.whl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelName<'a> {
    /// The distribution.
    pub distribution: &'a str,
    /// The version.
    pub version: &'a str,
    /// The Python tag, as `cp313`.
    pub python: &'a str,
    /// The ABI tag, as `cp313` or `cp313t`.
    pub abi: &'a str,
    /// The platform tag, as `macosx_11_0_arm64`.
    pub platform: &'a str,
}

impl<'a> WheelName<'a> {
    /// Parse a wheel file name, or None for anything else.
    pub fn parse(name: &'a str) -> Option<Self> {
        let stem = name.strip_suffix(".whl")?;
        let parts: Vec<&str> = stem.split('-').collect();

        match parts.as_slice() {
            [distribution, version, python, abi, platform] => Some(Self {
                distribution,
                version,
                python,
                abi,
                platform,
            }),
            _ => None,
        }
    }

    /// Whether this wheel runs on the probed interpreter: its tag, and a
    /// platform tag the machine satisfies.
    pub fn fits(&self, probe: &Probe) -> bool {
        let version = probe.parsed_version();
        let python = format!("cp{}{}", version.major(), version.minor());

        if self.python != python || self.abi != probe.tag() {
            return false;
        }

        platform_fits(self.platform, probe)
    }
}

/// Whether a platform tag is satisfied by the machine the probe describes.
fn platform_fits(tag: &str, probe: &Probe) -> bool {
    let machine = match probe.machine.as_str() {
        "arm64" | "aarch64" => "aarch64",
        "x86_64" | "amd64" | "AMD64" => "x86_64",
        other => other,
    };

    if let Some(rest) = tag.strip_prefix("macosx_") {
        if probe.platform != "darwin" {
            return false;
        }

        // macosx_11_0_arm64: the major and minor the wheel needs, then the
        // architecture, which macOS spells arm64.

        let mut parts = rest.splitn(3, '_');
        let major = parts.next().and_then(|part| part.parse::<u64>().ok());
        let minor = parts.next().and_then(|part| part.parse::<u64>().ok());
        let arch = parts.next().unwrap_or_default();
        let arch = if arch == "arm64" { "aarch64" } else { arch };

        if arch != machine && arch != "universal2" {
            return false;
        }

        return match (major, minor, Version::parse(&probe.mac_version)) {
            (Some(major), Some(minor), Some(have)) => {
                (have.major(), have.minor()) >= (major, minor)
            }
            _ => true,
        };
    }

    if probe.platform != "linux" {
        return false;
    }

    let (family, rest) = if let Some(rest) = tag.strip_prefix("manylinux_") {
        ("glibc", rest)
    } else if let Some(rest) = tag.strip_prefix("musllinux_") {
        ("musl", rest)
    } else {
        return false;
    };

    // manylinux_2_28_x86_64: the libc version the wheel needs, then the
    // architecture. An empty libc from the probe is taken as glibc, which
    // is what platform.libc_ver() fails to report on some systems.

    let mut parts = rest.splitn(3, '_');
    let major = parts.next().and_then(|part| part.parse::<u64>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u64>().ok());
    let arch = parts.next().unwrap_or_default();

    let libc = if probe.libc.is_empty() {
        "glibc"
    } else {
        probe.libc.as_str()
    };

    if arch != machine || libc != family {
        return false;
    }

    match (major, minor, Version::parse(&probe.libc_version)) {
        (Some(major), Some(minor), Some(have)) => (have.major(), have.minor()) >= (major, minor),
        _ => true,
    }
}

/// Pick the wheel for `version` that fits the probed interpreter from
/// `names`, preferring the most specific platform (the highest macOS or
/// libc version it may ask for).
pub fn select_wheel<'a>(
    names: impl IntoIterator<Item = &'a str>,
    version: &str,
    probe: &Probe,
) -> Option<&'a str> {
    let mut fitting: Vec<&'a str> = names
        .into_iter()
        .filter(|name| {
            WheelName::parse(name).is_some_and(|wheel| {
                wheel.distribution == DISTRIBUTION && wheel.version == version && wheel.fits(probe)
            })
        })
        .collect();

    fitting.sort();
    fitting.pop()
}

/// The directory for one version and tag under a runtime root:
/// `ROOT/VERSION/TAG`, holding the unpacked wheel.
pub fn runtime_dir(root: &Path, version: &str, tag: &str) -> PathBuf {
    root.join(version).join(tag)
}

/// How the runtime library was provided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// The environment's own `muru` matches muructl's version; nothing is
    /// injected.
    Environment,
    /// An unpacked wheel that was already there.
    Directory,
    /// An unpacked wheel fetched now.
    Fetched,
    /// The environment's own `muru` of another version, because nothing
    /// matching could be had.
    Fallback,
}

/// The outcome: what goes on PYTHONPATH, if anything, and what to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provided {
    /// The directory to put first on PYTHONPATH, or none when the
    /// environment's own copy is used.
    pub pythonpath: Option<PathBuf>,
    /// The version the host will import.
    pub version: String,
    /// How it was provided.
    pub how: How,
    /// Lines worth telling: what was fetched, and the warning of a
    /// fallback.
    pub notes: Vec<String>,
}

impl Provided {
    /// One line saying where the runtime library comes from.
    pub fn describe(&self) -> String {
        match (&self.how, &self.pythonpath) {
            (How::Environment, _) => {
                format!("runtime library {} from the environment", self.version)
            }
            (How::Fallback, _) => format!(
                "runtime library {} from the environment, which does not match muructl",
                self.version
            ),
            (_, Some(path)) => format!("runtime library {} from {}", self.version, path.display()),
            (_, None) => format!("runtime library {}", self.version),
        }
    }
}

/// What providing the runtime library needs.
#[derive(Debug, Clone)]
pub struct Options<'a> {
    /// The interpreter, probed.
    pub probe: &'a Probe,
    /// muructl's version as PEP 440 spells it.
    pub version: &'a str,
    /// The runtime root: CACHE/runtime in development, STATE/runtime under
    /// the daemon.
    pub root: &'a Path,
    /// A local directory of wheels, looked in before the network.
    pub wheel_dir: Option<&'a Path>,
    /// Never try the network.
    pub offline: bool,
}

/// Why the runtime library could not be provided.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// A request failed.
    #[error("fetching {url}: {message}")]
    Fetch {
        /// The URL.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// No wheel fits.
    #[error("no muru {version} wheel for {tag} on {platform} {looked}")]
    NoWheel {
        /// The version wanted.
        version: String,
        /// The interpreter tag.
        tag: String,
        /// The platform, as the probe describes it.
        platform: String,
        /// Where was looked.
        looked: String,
    },
    /// The downloaded file does not match its published hash.
    #[error("{filename} did not match the hash PyPI published for it, so it was not installed")]
    Digest {
        /// The wheel.
        filename: String,
    },
    /// The wheel could not be unpacked.
    #[error("unpacking {filename} into {target}: {message}")]
    Unpack {
        /// The wheel.
        filename: String,
        /// Where.
        target: PathBuf,
        /// What went wrong.
        message: String,
    },
    /// A file operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The error.
        source: std::io::Error,
    },
    /// Nothing usable exists and nothing could be fetched.
    #[error(
        "the runtime library muru {version} for {tag} is not available: {reason}; put its wheel in \
         a directory named by {wheel_dir}, or install muru=={version} into the environment"
    )]
    Unavailable {
        /// The version wanted.
        version: String,
        /// The interpreter tag.
        tag: String,
        /// Why.
        reason: String,
        /// How a wheel directory is named in this mode.
        wheel_dir: &'static str,
    },
}

/// Provide the runtime library for the interpreter: use what exists, go
/// to the network last, fall back to a mismatched copy with a warning.
/// `wheel_dir_setting` names the wheel directory setting of the mode, for
/// the message when nothing can be had.
pub async fn provide(
    options: &Options<'_>,
    wheel_dir_setting: &'static str,
) -> Result<Provided, RuntimeError> {
    let tag = options.probe.tag();
    let installed = options.probe.muru.as_ref();

    if let Some(found) = installed
        && found.version == options.version
    {
        return Ok(Provided {
            pythonpath: None,
            version: found.version.clone(),
            how: How::Environment,
            notes: Vec::new(),
        });
    }

    let dir = runtime_dir(options.root, options.version, &tag);

    if dir.join(DISTRIBUTION).is_dir() {
        return Ok(Provided {
            pythonpath: Some(dir),
            version: options.version.to_owned(),
            how: How::Directory,
            notes: Vec::new(),
        });
    }

    match install(options, &dir).await {
        Ok(filename) => Ok(Provided {
            pythonpath: Some(dir),
            version: options.version.to_owned(),
            how: How::Fetched,
            notes: vec![format!("fetched the runtime library {filename}, once")],
        }),
        Err(error) => match installed {
            Some(found) => Ok(Provided {
                pythonpath: None,
                version: found.version.clone(),
                how: How::Fallback,
                notes: vec![format!(
                    "the environment's muru is {} and muructl is {}; using the environment's, \
                     since the matching one could not be had ({error})",
                    found.version, options.version
                )],
            }),
            None => Err(RuntimeError::Unavailable {
                version: options.version.to_owned(),
                tag,
                reason: error.to_string(),
                wheel_dir: wheel_dir_setting,
            }),
        },
    }
}

/// Install the wheel for the probed interpreter into `target`, which then
/// holds the `muru` package and goes on PYTHONPATH: from the wheel
/// directory when it has one, else from PyPI unless offline, with the
/// published hash verified. Returns the wheel's file name.
pub async fn install(options: &Options<'_>, target: &Path) -> Result<String, RuntimeError> {
    let tag = options.probe.tag();
    let platform = format!("{} {}", options.probe.platform, options.probe.machine);

    if let Some(wheel_dir) = options.wheel_dir {
        let names = list_wheels(wheel_dir)?;
        let found = select_wheel(
            names.iter().map(String::as_str),
            options.version,
            options.probe,
        );

        if let Some(name) = found {
            let bytes = std::fs::read(wheel_dir.join(name)).map_err(|source| RuntimeError::Io {
                path: wheel_dir.join(name),
                source,
            })?;
            unpack(options.probe, name, &bytes, target)?;
            return Ok(name.to_owned());
        }

        if options.offline {
            return Err(RuntimeError::NoWheel {
                version: options.version.to_owned(),
                tag,
                platform,
                looked: format!("in {}", wheel_dir.display()),
            });
        }
    } else if options.offline {
        return Err(RuntimeError::NoWheel {
            version: options.version.to_owned(),
            tag,
            platform,
            looked: "and no wheel directory to look in while offline".to_owned(),
        });
    }

    let (filename, url, sha256) = published_wheel(options.version, options.probe).await?;
    let bytes = fetch(&url).await?;

    if hex_sha256(&bytes) != sha256 {
        return Err(RuntimeError::Digest { filename });
    }

    unpack(options.probe, &filename, &bytes, target)?;
    Ok(filename)
}

/// The wheel files in a directory.
fn list_wheels(dir: &Path) -> Result<Vec<String>, RuntimeError> {
    let entries = std::fs::read_dir(dir).map_err(|source| RuntimeError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    Ok(entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".whl"))
        .collect())
}

/// The wheel PyPI publishes for the version and interpreter: its name,
/// URL and sha256.
async fn published_wheel(
    version: &str,
    probe: &Probe,
) -> Result<(String, String, String), RuntimeError> {
    #[derive(serde::Deserialize)]
    struct Listing {
        urls: Vec<File>,
    }

    #[derive(serde::Deserialize)]
    struct File {
        filename: String,
        url: String,
        digests: Digests,
    }

    #[derive(serde::Deserialize)]
    struct Digests {
        sha256: String,
    }

    let url = format!("https://pypi.org/pypi/{DISTRIBUTION}/{version}/json");
    let body = fetch(&url).await?;
    let listing: Listing = serde_json::from_slice(&body).map_err(|error| RuntimeError::Fetch {
        url: url.clone(),
        message: format!("unexpected answer ({error})"),
    })?;
    let chosen = select_wheel(
        listing.urls.iter().map(|file| file.filename.as_str()),
        version,
        probe,
    )
    .ok_or_else(|| RuntimeError::NoWheel {
        version: version.to_owned(),
        tag: probe.tag(),
        platform: format!("{} {}", probe.platform, probe.machine),
        looked: "on PyPI".to_owned(),
    })?
    .to_owned();
    let file = listing
        .urls
        .into_iter()
        .find(|file| file.filename == chosen)
        .expect("the chosen wheel is in the listing");

    Ok((file.filename, file.url, file.digests.sha256))
}

/// GET a URL over HTTPS, within the timeouts.
async fn fetch(url: &str) -> Result<Vec<u8>, RuntimeError> {
    use http_body_util::BodyExt;

    let failed = |message: String| RuntimeError::Fetch {
        url: url.to_owned(),
        message,
    };
    let uri: http::Uri = url.parse().map_err(|error| failed(format!("{error}")))?;
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
    connector.enforce_http(false);
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .wrap_connector(connector);
    let client: hyper_util::client::legacy::Client<_, http_body_util::Empty<bytes::Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(connector);
    let request = http::Request::get(uri)
        .header(
            http::header::USER_AGENT,
            format!("muructl/{}", pep440(env!("CARGO_PKG_VERSION"))),
        )
        .body(http_body_util::Empty::new())
        .map_err(|error| failed(format!("{error}")))?;
    let response = tokio::time::timeout(DOWNLOAD_TIMEOUT, client.request(request))
        .await
        .map_err(|_| failed(format!("no answer within {}s", DOWNLOAD_TIMEOUT.as_secs())))?
        .map_err(|error| failed(format!("{error}")))?;

    if !response.status().is_success() {
        return Err(failed(format!("HTTP {}", response.status())));
    }

    let body = tokio::time::timeout(DOWNLOAD_TIMEOUT, response.into_body().collect())
        .await
        .map_err(|_| {
            failed(format!(
                "not received within {}s",
                DOWNLOAD_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| failed(format!("{error}")))?;

    Ok(body.to_bytes().to_vec())
}

/// The SHA-256 of some bytes, as lowercase hex.
fn hex_sha256(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Unpack a wheel into `target` with the interpreter's own zipfile module,
/// so no archive code is linked in. The wheel is unpacked beside the
/// target and renamed into place, so a half-finished directory is never
/// seen; an existing target is filled in place.
fn unpack(probe: &Probe, filename: &str, bytes: &[u8], target: &Path) -> Result<(), RuntimeError> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| RuntimeError::Io { path, source }
    };
    let parent = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(io(parent))?;
    let staging = parent.join(format!(
        ".{}.tmp-{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("runtime"),
        std::process::id()
    ));
    let wheel = parent.join(filename);
    std::fs::write(&wheel, bytes).map_err(io(&wheel))?;
    let into = if target.is_dir() { target } else { &staging };
    let output = std::process::Command::new(&probe.executable)
        .args(["-I", "-m", "zipfile", "-e"])
        .arg(&wheel)
        .arg(into)
        .output();
    let _ = std::fs::remove_file(&wheel);
    let output = output.map_err(|source| RuntimeError::Io {
        path: probe.executable.clone(),
        source,
    })?;

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&staging);

        return Err(RuntimeError::Unpack {
            filename: filename.to_owned(),
            target: target.to_path_buf(),
            message: String::from_utf8_lossy(&output.stderr)
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .unwrap_or("no output")
                .to_owned(),
        });
    }

    if into == staging {
        std::fs::rename(&staging, target).map_err(io(target))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(platform: &str, machine: &str, libc: &str, libc_version: &str) -> Probe {
        Probe {
            version: "3.13.9".to_owned(),
            implementation: "cpython".to_owned(),
            cache_tag: "cpython-313".to_owned(),
            free_threaded: false,
            platform: platform.to_owned(),
            machine: machine.to_owned(),
            libc: libc.to_owned(),
            libc_version: libc_version.to_owned(),
            mac_version: if platform == "darwin" {
                "15.1".to_owned()
            } else {
                String::new()
            },
            prefix: PathBuf::from("/usr"),
            base_prefix: PathBuf::from("/usr"),
            executable: PathBuf::from("/usr/bin/python3"),
            muru: None,
        }
    }

    const NAMES: [&str; 8] = [
        "muru-0.1.0a12-cp313-cp313-macosx_10_12_x86_64.whl",
        "muru-0.1.0a12-cp313-cp313-macosx_11_0_arm64.whl",
        "muru-0.1.0a12-cp313-cp313-manylinux_2_28_aarch64.whl",
        "muru-0.1.0a12-cp313-cp313-manylinux_2_28_x86_64.whl",
        "muru-0.1.0a12-cp313-cp313-musllinux_1_2_x86_64.whl",
        "muru-0.1.0a12-cp314-cp314t-manylinux_2_28_x86_64.whl",
        "muru-0.1.0a11-cp313-cp313-manylinux_2_28_x86_64.whl",
        "muru-0.1.0a12.tar.gz",
    ];

    #[test]
    fn cargo_versions_become_pep_440() {
        assert_eq!(pep440("0.1.0-alpha.12"), "0.1.0a12");
        assert_eq!(pep440("0.2.0-beta.1"), "0.2.0b1");
        assert_eq!(pep440("1.0.0-rc.2"), "1.0.0rc2");
        assert_eq!(pep440("1.0.0"), "1.0.0");
    }

    #[test]
    fn the_wheel_for_the_machine_is_chosen() {
        let names = NAMES.iter().copied();
        assert_eq!(
            select_wheel(names.clone(), "0.1.0a12", &probe("darwin", "arm64", "", "")),
            Some("muru-0.1.0a12-cp313-cp313-macosx_11_0_arm64.whl")
        );
        assert_eq!(
            select_wheel(
                names.clone(),
                "0.1.0a12",
                &probe("linux", "x86_64", "glibc", "2.35")
            ),
            Some("muru-0.1.0a12-cp313-cp313-manylinux_2_28_x86_64.whl")
        );
        assert_eq!(
            select_wheel(
                names.clone(),
                "0.1.0a12",
                &probe("linux", "x86_64", "musl", "1.2.4")
            ),
            Some("muru-0.1.0a12-cp313-cp313-musllinux_1_2_x86_64.whl")
        );
        assert_eq!(
            select_wheel(
                names.clone(),
                "0.1.0a12",
                &probe("linux", "aarch64", "glibc", "2.31")
            ),
            Some("muru-0.1.0a12-cp313-cp313-manylinux_2_28_aarch64.whl")
        );
        assert_eq!(
            select_wheel(
                names.clone(),
                "0.1.0a12",
                &probe("linux", "x86_64", "glibc", "2.17")
            ),
            None,
            "an old glibc cannot run a manylinux_2_28 wheel"
        );
        assert_eq!(
            select_wheel(names, "0.1.0a13", &probe("darwin", "arm64", "", "")),
            None
        );

        let mut free_threaded = probe("linux", "x86_64", "glibc", "2.35");
        free_threaded.version = "3.14.0".to_owned();
        free_threaded.free_threaded = true;
        assert_eq!(
            select_wheel(NAMES.iter().copied(), "0.1.0a12", &free_threaded),
            Some("muru-0.1.0a12-cp314-cp314t-manylinux_2_28_x86_64.whl")
        );
    }

    #[test]
    fn a_matching_environment_is_used_and_nothing_fetched() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut found = probe("darwin", "arm64", "", "");
        found.muru = Some(crate::probe::MuruImport {
            version: "0.1.0a12".to_owned(),
            path: String::new(),
        });
        let options = Options {
            probe: &found,
            version: "0.1.0a12",
            root: dir.path(),
            wheel_dir: None,
            offline: true,
        };
        let provided = runtime
            .block_on(provide(&options, "MURU_WHEEL_DIR"))
            .unwrap();

        assert_eq!(provided.how, How::Environment);
        assert_eq!(provided.pythonpath, None);
        assert_eq!(
            provided.describe(),
            "runtime library 0.1.0a12 from the environment"
        );

        // A mismatched environment is the fallback when nothing can be
        // fetched, with a warning; with no environment copy at all the
        // failure names the ways out.

        let mut mismatched = found.clone();
        mismatched.muru.as_mut().unwrap().version = "0.1.0a11".to_owned();
        let options = Options {
            probe: &mismatched,
            version: "0.1.0a12",
            root: dir.path(),
            wheel_dir: None,
            offline: true,
        };
        let provided = runtime
            .block_on(provide(&options, "MURU_WHEEL_DIR"))
            .unwrap();
        assert_eq!(provided.how, How::Fallback);
        assert_eq!(provided.version, "0.1.0a11");
        assert!(provided.notes[0].contains("could not be had"));

        let mut without = found.clone();
        without.muru = None;
        let options = Options {
            probe: &without,
            version: "0.1.0a12",
            root: dir.path(),
            wheel_dir: None,
            offline: true,
        };
        let error = runtime
            .block_on(provide(&options, "MURU_WHEEL_DIR"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("install muru==0.1.0a12 into the environment")
        );
        assert!(error.to_string().contains("MURU_WHEEL_DIR"));
    }

    #[test]
    fn a_wheel_in_the_wheel_directory_is_unpacked_with_the_interpreter() {
        let Some(python3) = crate::uv::find_on_path("python3") else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let found = crate::probe::probe(&python3).unwrap();
        let version = found.parsed_version();
        let tag = found.tag();

        let platform = if found.platform == "darwin" {
            format!(
                "macosx_11_0_{}",
                if found.machine == "arm64" {
                    "arm64"
                } else {
                    "x86_64"
                }
            )
        } else {
            format!("manylinux_2_28_{}", found.machine)
        };
        let name = format!(
            "muru-9.9.9-cp{}{}-{tag}-{platform}.whl",
            version.major(),
            version.minor()
        );

        // A wheel is a zip: one made with the interpreter's zipfile holding
        // a muru package with a marker file.

        let wheel_dir = dir.path().join("wheels");
        let source = dir.path().join("source").join("muru");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&wheel_dir).unwrap();
        std::fs::write(source.join("__init__.py"), "MARKER = 1\n").unwrap();
        let status = std::process::Command::new(&python3)
            .args(["-I", "-m", "zipfile", "-c"])
            .arg(wheel_dir.join(&name))
            .arg(&source)
            .status()
            .unwrap();
        assert!(status.success());

        let options = Options {
            probe: &found,
            version: "9.9.9",
            root: &dir.path().join("runtime"),
            wheel_dir: Some(&wheel_dir),
            offline: true,
        };
        let provided = runtime
            .block_on(provide(&options, "MURU_WHEEL_DIR"))
            .unwrap();

        assert_eq!(provided.how, How::Fetched);
        let path = provided.pythonpath.clone().unwrap();
        assert!(path.join("muru").join("__init__.py").is_file());
        assert_eq!(
            path,
            runtime_dir(&dir.path().join("runtime"), "9.9.9", &tag)
        );
        assert!(provided.notes[0].starts_with("fetched the runtime library muru-9.9.9"));

        // The second time it is already there.

        let again = runtime
            .block_on(provide(&options, "MURU_WHEEL_DIR"))
            .unwrap();
        assert_eq!(again.how, How::Directory);
        assert_eq!(again.pythonpath, provided.pythonpath);
    }
}
