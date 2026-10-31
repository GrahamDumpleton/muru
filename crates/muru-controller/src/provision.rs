//! What the daemon gives provisioning (design specification section 8.4):
//! uv as it was found, the isolation the daemon runs it with, and the
//! `[tools]` table behind both.

use std::path::Path;

use muru_provision::{Isolation, Uv};

/// The daemon's provisioning context.
#[derive(Debug, Clone)]
pub struct Provisioning {
    /// uv, when it was found.
    pub uv: Option<Uv>,
    /// How uv is run for interpreter operations: isolated directories
    /// under the state directory, no project configuration.
    pub isolation: Isolation,
    /// The `[tools]` table.
    pub tools: muru_config::Tools,
    /// Whether interpreters are resolved and probed. Off for pure
    /// resolution, as in tests, where a request is left as it is.
    pub active: bool,
}

impl Provisioning {
    /// No uv and no probing: a path is taken as given and a request is
    /// left for the host to fail on, as before provisioning existed.
    pub fn off() -> Self {
        Self {
            uv: None,
            isolation: Isolation::default(),
            tools: muru_config::Tools::default(),
            active: false,
        }
    }

    /// The daemon's context from its `[tools]` table and state directory.
    pub fn for_daemon(tools: &muru_config::Tools, state_dir: &Path) -> Self {
        let tools_dir = state_dir.join("tools");
        let uv = Uv::find(tools.uv.as_deref().map(Path::new), Some(&tools_dir));
        let offline = tools.offline.unwrap_or(false);
        let isolation = Isolation {
            python_install_dir: Some(
                tools
                    .python_install_dir
                    .as_deref()
                    .map(Into::into)
                    .unwrap_or_else(|| state_dir.join("python")),
            ),
            cache_dir: Some(state_dir.join("uv-cache")),
            no_downloads: offline
                || tools.python_downloads == Some(muru_config::PythonDownloads::Never),
            offline,
            no_config: true,
            config_file: None,
        };

        Self {
            uv,
            isolation,
            tools: tools.clone(),
            active: true,
        }
    }

    /// Whether a Python the node lacks may be downloaded for a release:
    /// `tools.python_downloads` is not never and the node is not offline.
    pub fn downloads_allowed(&self) -> bool {
        self.active && !self.isolation.no_downloads
    }

    /// Where the environment built for a release lives (section 8.2).
    pub fn environment_dir(state_dir: &Path, app: &str, id: &str) -> std::path::PathBuf {
        state_dir.join("envs").join(app).join(id)
    }

    /// Where a release's redirected bytecode goes (section 8.2).
    pub fn pycache_dir(state_dir: &Path, app: &str, id: &str) -> std::path::PathBuf {
        state_dir.join("pycache").join(app).join(id)
    }

    /// The isolation a build runs with: the daemon's directories, the
    /// project's own uv configuration kept (no `UV_NO_CONFIG`), the
    /// daemon's config file when `tools.uv_config` names one, and a cache
    /// of the release user's own, since hard links need files it can read.
    pub fn build_isolation(&self, cache_dir: std::path::PathBuf) -> Isolation {
        Isolation {
            cache_dir: Some(cache_dir),
            no_config: false,
            config_file: self.tools.uv_config.as_deref().map(Into::into),
            ..self.isolation.clone()
        }
    }

    /// What the daemon says about uv at start-up: where it was found and
    /// its version, or that it was not found and what that means.
    pub fn describe_uv(&self) -> String {
        match &self.uv {
            Some(uv) => {
                let version = uv
                    .version()
                    .map(|version| version.to_string())
                    .unwrap_or_else(|error| format!("version unknown: {error}"));
                let mut text = format!(
                    "uv {version} at {} (found through {})",
                    uv.path.display(),
                    uv.found.as_str()
                );

                if let Some(wanted) = self
                    .tools
                    .uv_version
                    .as_deref()
                    .and_then(|text| muru_provision::SpecifierSet::parse(text).ok())
                    && let Ok(version) = uv.version()
                    && !wanted.contains(&version)
                {
                    text.push_str(&format!(
                        "; tools.uv_version asks for {wanted}, so what this version lacks is reported when it is needed"
                    ));
                }

                text
            }
            None => {
                "uv was not found (tools.uv, MURU_UV, beside muructl, PATH, the managed copy); \
                     releases run on existing environments, and a Python version request cannot be \
                     resolved until uv is installed"
                    .to_owned()
            }
        }
    }
}
