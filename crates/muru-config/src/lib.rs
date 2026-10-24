//! Configuration schema types, TOML parsing, validation, precedence merge and JSON Schema output.
//!
//! Every user-visible setting is defined once, here, and the command line,
//! configuration files and later the admin API all map onto these types
//! (design specification sections 3.3 and 14.4). Phase 1 covers the
//! application-shipped defaults of section 6.8: the `[tool.muru]` table in a
//! project's pyproject.toml, or the same keys at the top of a muru.toml.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The interface an entrypoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Interface {
    /// Detect from the callable: a coroutine function or an object with an
    /// `async def __call__` is ASGI, anything else WSGI.
    Auto,
    /// PEP 3333.
    Wsgi,
    /// ASGI 3.
    Asgi,
}

/// Whether the application is imported in the host before forking (section 6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preload {
    /// Preload when the import check passes, otherwise import per worker.
    Auto,
    /// Preload, and fail the release when the import check does not pass.
    True,
    /// Import in every worker.
    False,
}

/// When a factory entrypoint is called (section 6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactoryPhase {
    /// In each worker, after the post_fork hook.
    Worker,
    /// Once in the host, during preload.
    Host,
}

macro_rules! words {
    ($type:ident { $($variant:ident => $word:literal),+ $(,)? }) => {
        impl $type {
            /// The setting's value as it is written.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $word,)+
                }
            }

            /// Every accepted value.
            pub const ALL: &'static [&'static str] = &[$($word),+];
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $type {
            type Err = ParseError;

            fn from_str(text: &str) -> Result<Self, ParseError> {
                match text {
                    $($word => Ok(Self::$variant),)+
                    _ => Err(ParseError {
                        setting: stringify!($type),
                        value: text.to_owned(),
                        accepted: Self::ALL,
                    }),
                }
            }
        }
    };
}

words!(Interface { Auto => "auto", Wsgi => "wsgi", Asgi => "asgi" });
words!(Preload { Auto => "auto", True => "true", False => "false" });
words!(FactoryPhase { Worker => "worker", Host => "host" });

/// A value that is not one of a setting's accepted words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not a {setting}: expected one of {}", accepted.join(", "))]
pub struct ParseError {
    /// The setting's type.
    pub setting: &'static str,
    /// What was given.
    pub value: String,
    /// What is accepted.
    pub accepted: &'static [&'static str],
}

/// Parse a size such as 10MB, 512KB, 1GB or a plain number of bytes.
/// KB, MB and GB are powers of 1024, as mod_wsgi counts them (ADR 0008);
/// case does not matter and a lone K, M or G is accepted.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let upper = text.to_ascii_uppercase();
    let (number, scale) = if let Some(number) = upper.strip_suffix("GB").or(upper.strip_suffix('G'))
    {
        (number, 1024u64 * 1024 * 1024)
    } else if let Some(number) = upper.strip_suffix("MB").or(upper.strip_suffix('M')) {
        (number, 1024 * 1024)
    } else if let Some(number) = upper.strip_suffix("KB").or(upper.strip_suffix('K')) {
        (number, 1024)
    } else if let Some(number) = upper.strip_suffix('B') {
        (number, 1)
    } else {
        (upper.as_str(), 1)
    };
    let value: u64 = number
        .trim()
        .parse()
        .map_err(|_| format!("{text:?} is not a size such as 10MB or 512KB"))?;
    value
        .checked_mul(scale)
        .ok_or_else(|| format!("{text:?} is too large"))
}

/// Write a size the way the configuration accepts it: the largest unit
/// that divides it exactly, else bytes.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "KB")];
    for (scale, unit) in UNITS {
        if bytes != 0 && bytes.is_multiple_of(scale) {
            return format!("{}{unit}", bytes / scale);
        }
    }
    bytes.to_string()
}

/// Parse a duration such as 250ms, 1.5s, 2m, 1h or 2 (seconds), as the
/// configuration and the command line write them.
pub fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
    let text = text.trim();
    let (number, scale) = if let Some(number) = text.strip_suffix("ms") {
        (number, 0.001)
    } else if let Some(number) = text.strip_suffix('h') {
        (number, 3600.0)
    } else if let Some(number) = text.strip_suffix('m') {
        (number, 60.0)
    } else if let Some(number) = text.strip_suffix('s') {
        (number, 1.0)
    } else {
        (text, 1.0)
    };

    let value: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("{text:?} is not a duration, for example 250ms, 1.5s or 2m"))?;
    if value < 0.0 || !value.is_finite() {
        return Err(format!("{text:?} is not a duration"));
    }
    Ok(std::time::Duration::from_secs_f64(value * scale))
}

/// Write a duration the way the configuration accepts it: whole seconds
/// as `5s`, otherwise milliseconds as `250ms`.
pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis != 0 && millis.is_multiple_of(1000) {
        format!("{}s", millis / 1000)
    } else {
        format!("{millis}ms")
    }
}

/// A release definition, the `[app.NAME.release.ID]` table (appendix C).
/// Phase 1 holds the keys a development run uses; `muructl run
/// --print-config` writes one so a laptop setup can become a declared
/// release.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// The application as module:callable.
    pub entrypoint: String,
    /// The interface, when declared rather than detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<Interface>,
    /// Whether the entrypoint is a factory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory: Option<bool>,
    /// When a factory is called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory_phase: Option<FactoryPhase>,
    /// The interpreter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// Worker processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<usize>,
    /// Application threads per worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<usize>,
    /// Threads for synchronous code called from an ASGI application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_threads: Option<usize>,
    /// Requests an ASGI worker's event loop takes at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// The preload mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preload: Option<Preload>,
    /// The hooks module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<String>,
}

/// The `[limits]` table (appendix C): sizes and durations as the
/// configuration writes them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Largest request body, for example 10MB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_size: Option<String>,
    /// Largest request head, for example 64KB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_header_bytes: Option<String>,
    /// Most headers in a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_headers: Option<usize>,
    /// Time allowed for a complete request head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_read_timeout: Option<String>,
    /// Time a request may wait for a worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_timeout: Option<String>,
    /// Time a request may take to produce its response head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_deadline: Option<String>,
    /// Time a client may pause while sending a body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_idle_timeout: Option<String>,
    /// Time a worker may pause mid-response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_idle_timeout: Option<String>,
}

/// A listener, one of the `[[listener]]` entries (appendix C).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    /// The address, as IP:PORT.
    pub address: String,
    /// Whether TLS is terminated on it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls: bool,
}

/// An application's table and its releases.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    /// Releases by identifier.
    #[serde(default)]
    pub release: BTreeMap<String, Release>,
}

/// A declarative configuration file as `muructl apply` will read it: the
/// subset a development run can describe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    /// Listeners.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listener: Vec<Listener>,
    /// Limits in force everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
    /// Applications by name.
    #[serde(default)]
    pub app: BTreeMap<String, App>,
}

impl Document {
    /// The document as TOML.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }
}

/// The `[tool.muru]` table: defaults an application ships with (section
/// 6.8). Every field is optional; what a command line or a release
/// definition sets takes precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolMuru {
    /// The application as module:callable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    /// The interface the entrypoint speaks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<Interface>,
    /// Whether the entrypoint is a factory returning the application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory: Option<bool>,
    /// When a factory is called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory_phase: Option<FactoryPhase>,
    /// Worker processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<usize>,
    /// Application threads per worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<usize>,
    /// Threads for synchronous code called from an ASGI application
    /// (section 6.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_threads: Option<usize>,
    /// Requests an ASGI worker's event loop takes at once (section 6.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// The preload mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preload: Option<Preload>,
    /// The module providing the lifecycle hooks (section 6.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<String>,
    /// The Python requirement, for example "3.13" or ">=3.12".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
}

/// Why a project's configuration could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// The file exists but could not be read.
    #[error("reading {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The file is not valid TOML, or the table has a wrong or unknown key.
    #[error("{path}: {message}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong, as toml reports it.
        message: String,
    },
}

/// The names of a project's configuration files, in the order they are tried.
pub const PROJECT_FILES: [&str; 2] = ["muru.toml", "pyproject.toml"];

/// Read a project's `[tool.muru]` table from `dir`: muru.toml (the keys at
/// the top level) is tried first, then pyproject.toml. Returns the file
/// read and the table, or None when the project has neither file or a
/// pyproject.toml without the table.
pub fn load_project(dir: &Path) -> Result<Option<(PathBuf, ToolMuru)>, ProjectError> {
    for name in PROJECT_FILES {
        let path = dir.join(name);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(ProjectError::Read { path, source }),
        };
        let table = if name == "muru.toml" {
            Some(
                parse_muru_toml(&text).map_err(|message| ProjectError::Invalid {
                    path: path.clone(),
                    message,
                })?,
            )
        } else {
            parse_pyproject(&text).map_err(|message| ProjectError::Invalid {
                path: path.clone(),
                message,
            })?
        };
        if let Some(table) = table {
            return Ok(Some((path, table)));
        }
    }
    Ok(None)
}

/// Parse a muru.toml: the `[tool.muru]` keys at the top level.
pub fn parse_muru_toml(text: &str) -> Result<ToolMuru, String> {
    toml::from_str(text).map_err(|error| error.to_string())
}

/// The `[tool.muru]` table of a pyproject.toml, if it has one.
pub fn parse_pyproject(text: &str) -> Result<Option<ToolMuru>, String> {
    #[derive(Deserialize)]
    struct Pyproject {
        #[serde(default)]
        tool: Tool,
    }
    #[derive(Deserialize, Default)]
    struct Tool {
        #[serde(default)]
        muru: Option<ToolMuru>,
    }
    let project: Pyproject = toml::from_str(text).map_err(|error| error.to_string())?;
    Ok(project.tool.muru)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_round_trip() {
        for word in Preload::ALL {
            assert_eq!(word.parse::<Preload>().unwrap().as_str(), *word);
        }
        assert_eq!("asgi".parse::<Interface>(), Ok(Interface::Asgi));
        let error = "maybe".parse::<Preload>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "\"maybe\" is not a Preload: expected one of auto, true, false"
        );
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(parse_size("10MB"), Ok(10 * 1024 * 1024));
        assert_eq!(parse_size("512kb"), Ok(512 * 1024));
        assert_eq!(parse_size("1G"), Ok(1024 * 1024 * 1024));
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size(" 2 MB "), Ok(2 * 1024 * 1024));
        assert!(parse_size("ten").is_err());
        assert!(parse_size("99999999999999999999GB").is_err());
    }

    #[test]
    fn sizes_and_durations_are_written_in_the_largest_exact_unit() {
        assert_eq!(format_size(100 * 1024 * 1024), "100MB");
        assert_eq!(format_size(64 * 1024), "64KB");
        assert_eq!(format_size(1500), "1500");
        assert_eq!(format_duration(Duration::from_secs(300)), "300s");
        assert_eq!(format_duration(Duration::from_millis(250)), "250ms");
    }

    #[test]
    fn a_document_round_trips_through_toml() {
        let mut document = Document {
            listener: vec![Listener {
                address: "127.0.0.1:8080".to_owned(),
                tls: false,
            }],
            limits: Some(Limits {
                max_body_size: Some("100MB".to_owned()),
                ..Limits::default()
            }),
            ..Document::default()
        };
        document.app.insert(
            "shop".to_owned(),
            App {
                release: BTreeMap::from([(
                    "r1".to_owned(),
                    Release {
                        entrypoint: "shop.wsgi:application".to_owned(),
                        processes: Some(2),
                        preload: Some(Preload::Auto),
                        ..Release::default()
                    },
                )]),
            },
        );
        let text = document.to_toml().unwrap();
        assert!(text.contains("[app.shop.release.r1]"), "{text}");
        assert!(
            text.contains("entrypoint = \"shop.wsgi:application\""),
            "{text}"
        );
        assert!(text.contains("[[listener]]"), "{text}");
        let back: Document = toml::from_str(&text).unwrap();
        assert_eq!(back, document);
    }

    #[test]
    fn pyproject_table_is_optional_and_strict() {
        assert_eq!(parse_pyproject("[project]\nname = 'x'\n").unwrap(), None);
        let table = parse_pyproject(
            "[tool.muru]\nentrypoint = 'shop.wsgi:application'\nprocesses = 2\nfactory = true\nfactory_phase = 'host'\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(table.entrypoint.as_deref(), Some("shop.wsgi:application"));
        assert_eq!(table.processes, Some(2));
        assert_eq!(table.factory, Some(true));
        assert_eq!(table.factory_phase, Some(FactoryPhase::Host));
        let error = parse_pyproject("[tool.muru]\nworkers = 2\n").unwrap_err();
        assert!(error.contains("workers"), "{error}");
        let error = parse_pyproject("[tool.muru]\npreload = 'maybe'\n").unwrap_err();
        assert!(error.contains("maybe"), "{error}");
    }

    #[test]
    fn muru_toml_holds_the_keys_at_the_top() {
        let table = parse_muru_toml("entrypoint = 'app:app'\nthreads = 8\n").unwrap();
        assert_eq!(table.entrypoint.as_deref(), Some("app:app"));
        assert_eq!(table.threads, Some(8));
    }

    #[test]
    fn project_files_are_tried_in_order() {
        let dir = std::env::temp_dir().join(format!("muru-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_project(&dir).unwrap(), None);
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.muru]\nentrypoint = 'a:b'\n",
        )
        .unwrap();
        let (path, table) = load_project(&dir).unwrap().unwrap();
        assert!(path.ends_with("pyproject.toml"));
        assert_eq!(table.entrypoint.as_deref(), Some("a:b"));
        std::fs::write(dir.join("muru.toml"), "entrypoint = 'c:d'\n").unwrap();
        let (path, table) = load_project(&dir).unwrap().unwrap();
        assert!(path.ends_with("muru.toml"));
        assert_eq!(table.entrypoint.as_deref(), Some("c:d"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
