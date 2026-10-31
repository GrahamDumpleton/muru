//! What names an interpreter (design specification section 8.1): a path,
//! or a request in uv's syntax from one of the sources a project has, in
//! their order.

use std::path::{Path, PathBuf};

use crate::specifier::{SpecifierSet, Version};

/// A path or a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Python {
    /// An interpreter to use as it is.
    Path(PathBuf),
    /// A version request in uv's syntax: `3.13`, `3.14t`, `>=3.12`,
    /// `cpython@3.13`.
    Request(String),
}

impl Python {
    /// Read a `--python` value or a `python` key. A value with a directory
    /// separator, or that names an existing file, is a path; a relative
    /// path with a directory in it is the project's. Anything else is a
    /// request, which uv parses.
    pub fn parse(text: &str, project: &Path) -> Self {
        let text = text.trim();

        if text.contains('/') || text.starts_with('.') || text.starts_with('~') {
            let path = PathBuf::from(text);

            return Python::Path(if path.is_absolute() || path.components().count() < 2 {
                path
            } else {
                project.join(path)
            });
        }

        if Path::new(text).is_file() {
            return Python::Path(PathBuf::from(text));
        }

        Python::Request(text.to_owned())
    }
}

/// Where a request came from, highest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// `--python` on the command line.
    Flag,
    /// `python` in `[tool.muru]`, or on a variant.
    Table,
    /// The project's `.python-version` file, uv's own pin.
    VersionFile,
    /// `requires-python` in `[project]`, used to choose only when nothing
    /// else does.
    RequiresPython,
}

impl Source {
    /// How the source is named to a user.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Flag => "--python",
            Source::Table => "python in [tool.muru]",
            Source::VersionFile => ".python-version",
            Source::RequiresPython => "requires-python",
        }
    }
}

/// A request and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The request, in uv's syntax.
    pub text: String,
    /// Its source.
    pub source: Source,
}

impl Request {
    /// The version the request names outright, when it does: `3.13`,
    /// `3.14t` and `cpython@3.13.2` name one, `>=3.12` does not.
    pub fn version(&self) -> Option<Version> {
        let text = self.text.rsplit('@').next().unwrap_or(&self.text);

        if text.starts_with(|c: char| c.is_ascii_digit()) {
            Version::parse(text)
        } else {
            None
        }
    }
}

/// The requests a project makes, in order, and its `requires-python`
/// constraint: `flag` is `--python` when it is not a path, `table` the
/// `python` key, and the project directory holds `.python-version` and
/// `pyproject.toml`.
pub fn requests(
    flag: Option<&str>,
    table: Option<&str>,
    project: &Path,
) -> (Vec<Request>, Option<SpecifierSet>) {
    let mut found = Vec::new();

    if let Some(text) = flag {
        found.push(Request {
            text: text.to_owned(),
            source: Source::Flag,
        });
    }

    if let Some(text) = table {
        found.push(Request {
            text: text.to_owned(),
            source: Source::Table,
        });
    }

    if let Some(text) = version_file(project) {
        found.push(Request {
            text,
            source: Source::VersionFile,
        });
    }

    let requires = requires_python(project);

    if let Some(requires) = &requires {
        found.push(Request {
            text: requires.to_string(),
            source: Source::RequiresPython,
        });
    }

    (found, requires)
}

/// The first pin in the project's `.python-version`, if any.
pub fn version_file(project: &Path) -> Option<String> {
    let text = std::fs::read_to_string(project.join(".python-version")).ok()?;

    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
}

/// `requires-python` from the project's `pyproject.toml`, if it has one
/// that parses; an unparsable one is ignored here and reported by the
/// tools that own it.
pub fn requires_python(project: &Path) -> Option<SpecifierSet> {
    #[derive(serde::Deserialize)]
    struct PyProject {
        #[serde(default)]
        project: Option<ProjectTable>,
    }

    #[derive(serde::Deserialize)]
    struct ProjectTable {
        #[serde(default, rename = "requires-python")]
        requires_python: Option<String>,
    }

    let text = std::fs::read_to_string(project.join("pyproject.toml")).ok()?;
    let parsed: PyProject = toml::from_str(&text).ok()?;
    let requirement = parsed.project?.requires_python?;
    SpecifierSet::parse(&requirement).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_requests_are_told_apart() {
        let project = Path::new("/srv/shop");
        assert_eq!(
            Python::parse("3.13", project),
            Python::Request("3.13".to_owned())
        );
        assert_eq!(
            Python::parse(">=3.12", project),
            Python::Request(">=3.12".to_owned())
        );
        assert_eq!(
            Python::parse("cpython@3.14t", project),
            Python::Request("cpython@3.14t".to_owned())
        );
        assert_eq!(
            Python::parse(".venv/bin/python", project),
            Python::Path(PathBuf::from("/srv/shop/.venv/bin/python"))
        );
        assert_eq!(
            Python::parse("/usr/bin/python3", project),
            Python::Path(PathBuf::from("/usr/bin/python3"))
        );
        assert_eq!(
            Python::parse("python3", project),
            Python::Request("python3".to_owned())
        );
    }

    #[test]
    fn requests_come_in_source_order_with_the_constraint_last() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = 'shop'\nrequires-python = '>=3.12'\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(".python-version"), "# pinned\n3.13\n").unwrap();

        let (found, requires) = requests(Some("3.14"), Some("3.13t"), dir.path());
        let texts: Vec<(&str, Source)> = found
            .iter()
            .map(|request| (request.text.as_str(), request.source))
            .collect();

        assert_eq!(
            texts,
            vec![
                ("3.14", Source::Flag),
                ("3.13t", Source::Table),
                ("3.13", Source::VersionFile),
                (">=3.12", Source::RequiresPython),
            ]
        );
        assert_eq!(requires.unwrap().to_string(), ">=3.12");
        assert_eq!(found[1].version(), Version::parse("3.13"));
        assert_eq!(found[3].version(), None);
    }

    #[test]
    fn a_project_without_files_makes_no_requests() {
        let dir = tempfile::tempdir().unwrap();
        let (found, requires) = requests(None, None, dir.path());

        assert!(found.is_empty());
        assert!(requires.is_none());
    }
}
