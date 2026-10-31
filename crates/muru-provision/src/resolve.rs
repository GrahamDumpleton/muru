//! Choosing the interpreter a release runs on (design specification
//! section 8.1): a path as given, an existing environment as it is, a
//! request through uv, the previous release's, or python3 on PATH; then
//! the probe, the support check and `requires-python`.

use std::path::{Path, PathBuf};

use crate::interpreter::{self, Python, Request, Source};
use crate::probe::{Probe, ProbeError, probe};
use crate::specifier::SpecifierSet;
use crate::uv::{Isolation, Uv, UvError, find_on_path};

/// Who is resolving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `muructl run` and `muructl build`: the active virtualenv counts,
    /// and uv runs as the developer's own.
    Development,
    /// The daemon: no active virtualenv, isolated directories.
    Daemon,
}

/// What there is to choose from.
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    /// Who is resolving.
    pub mode: Mode,
    /// `--python`, a path or a request.
    pub flag: Option<&'a str>,
    /// `python` in `[tool.muru]`, or on the variant.
    pub table: Option<&'a str>,
    /// The project or source directory.
    pub project: &'a Path,
    /// The previous release's interpreter, under the daemon.
    pub previous: Option<&'a Path>,
    /// Whether the build mode is `always`, in which case a request is
    /// preferred over an existing environment.
    pub always: bool,
    /// uv, when it was found.
    pub uv: Option<&'a Uv>,
    /// How uv is run.
    pub isolation: &'a Isolation,
}

/// How the interpreter was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chosen {
    /// A path given by `--python` or the `python` key.
    Path(Source),
    /// The active virtualenv.
    ActiveVirtualenv,
    /// The project's own `.venv`.
    ProjectVirtualenv,
    /// A request resolved through uv.
    Request(Request),
    /// The previous release's interpreter.
    Previous,
    /// `python3` on PATH, because nothing else named one.
    Default,
}

/// The outcome: the interpreter, how it was chosen, and what the probe
/// found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The interpreter to run, with its directory canonical and its own
    /// name kept, since it is often a symlink into a virtualenv.
    pub python: PathBuf,
    /// How it was chosen.
    pub chosen: Chosen,
    /// What the probe found.
    pub probe: Probe,
    /// The project's `requires-python`, when it has one.
    pub requires_python: Option<SpecifierSet>,
}

impl Resolved {
    /// The request that chose the interpreter, if one did.
    pub fn request(&self) -> Option<&Request> {
        match &self.chosen {
            Chosen::Request(request) => Some(request),
            _ => None,
        }
    }

    /// One line saying what was chosen and from where, for the terminal
    /// or the release log.
    pub fn describe(&self) -> String {
        let how = match &self.chosen {
            Chosen::Path(source) => format!("from {}", source.as_str()),
            Chosen::ActiveVirtualenv => "the active virtualenv".to_owned(),
            Chosen::ProjectVirtualenv => "the project's .venv".to_owned(),
            Chosen::Request(request) => {
                format!("{} from {}", request.text, request.source.as_str())
            }
            Chosen::Previous => "the previous release's".to_owned(),
            Chosen::Default => "python3 on PATH, since nothing named one".to_owned(),
        };

        format!(
            "{} at {} ({how})",
            self.probe.describe(),
            self.python.display()
        )
    }
}

/// Why no interpreter could be settled on. Every message names what was
/// asked, what was found and what provides the missing piece.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// A request needs uv, and there is none.
    #[error(
        "Python {request} (from {origin}) needs uv to find it, and uv was not found on PATH, \
         beside muructl or in MURU_UV; install uv, or give --python the interpreter's path"
    )]
    NeedsUv {
        /// The request.
        request: String,
        /// Its source.
        origin: &'static str,
    },
    /// uv found no interpreter for the request.
    #[error(
        "no Python matching {request} (from {origin}) was found on this machine; `uv python \
         install {request}` adds one"
    )]
    NotFound {
        /// The request.
        request: String,
        /// Its source.
        origin: &'static str,
    },
    /// uv itself failed.
    #[error(transparent)]
    Uv(#[from] UvError),
    /// A request names a version outside `requires-python`.
    #[error(
        "Python {request} (from {origin}) is outside the project's requires-python {requirement}"
    )]
    Conflict {
        /// The request.
        request: String,
        /// Its source.
        origin: &'static str,
        /// The constraint.
        requirement: String,
    },
    /// The interpreter cannot run Muru.
    #[error("{python}: {reason}")]
    Unsupported {
        /// The interpreter.
        python: PathBuf,
        /// Why.
        reason: String,
    },
    /// The interpreter chosen is outside `requires-python`.
    #[error("{python} is Python {version} but the project requires {requirement}{hint}")]
    RequiresPython {
        /// The interpreter.
        python: PathBuf,
        /// Its version.
        version: String,
        /// The constraint.
        requirement: String,
        /// What to do, for a virtualenv that needs rebuilding.
        hint: String,
    },
    /// The interpreter could not be probed.
    #[error(transparent)]
    Probe(#[from] ProbeError),
    /// Nothing named an interpreter and there is no python3 on PATH.
    #[error(
        "no Python interpreter was found: nothing named one, there is no .venv in {project}, and \
         there is no python3 on PATH"
    )]
    NoPython {
        /// The project.
        project: PathBuf,
    },
}

/// Resolve the interpreter for `inputs`.
pub fn resolve(inputs: &Inputs<'_>) -> Result<Resolved, ResolveError> {
    let flag = inputs.flag.map(|text| Python::parse(text, inputs.project));
    let table = inputs.table.map(|text| Python::parse(text, inputs.project));
    let (requests, requires_python) = interpreter::requests(
        match &flag {
            Some(Python::Request(text)) => Some(text.as_str()),
            _ => None,
        },
        match &table {
            Some(Python::Request(text)) => Some(text.as_str()),
            _ => None,
        },
        inputs.project,
    );

    // A request from a higher source that violates requires-python is an
    // error before anything runs, as it is under uv.

    if let Some(requirement) = &requires_python {
        for request in &requests {
            if request.source == Source::RequiresPython {
                continue;
            }

            if let Some(version) = request.version()
                && !requirement.contains(&version)
            {
                return Err(ResolveError::Conflict {
                    request: request.text.clone(),
                    origin: request.source.as_str(),
                    requirement: requirement.to_string(),
                });
            }
        }
    }

    let (python, chosen) = choose(inputs, flag, table, &requests)?;
    let python = canonical_dir(python);
    let found = probe(&python)?;

    if let Some(reason) = found.unsupported() {
        return Err(ResolveError::Unsupported { python, reason });
    }

    if let Some(requirement) = &requires_python
        && !requirement.contains(&found.parsed_version())
    {
        let hint = if found.in_virtualenv() {
            "; rebuild the environment with uv sync, or muructl build".to_owned()
        } else {
            String::new()
        };

        return Err(ResolveError::RequiresPython {
            python,
            version: found.version.clone(),
            requirement: requirement.to_string(),
            hint,
        });
    }

    Ok(Resolved {
        python,
        chosen,
        probe: found,
        requires_python,
    })
}

/// The order of section 8.1: a path, an existing environment, a request,
/// the previous release's interpreter, python3.
fn choose(
    inputs: &Inputs<'_>,
    flag: Option<Python>,
    table: Option<Python>,
    requests: &[Request],
) -> Result<(PathBuf, Chosen), ResolveError> {
    if let Some(Python::Path(path)) = flag {
        return Ok((path, Chosen::Path(Source::Flag)));
    }

    if let Some(Python::Path(path)) = table {
        return Ok((path, Chosen::Path(Source::Table)));
    }

    // A request on the command line is answered before anything else is
    // looked at: --python 3.13 means that Python, whatever environment
    // exists. The project's own requests come after an environment that
    // exists, which is used as it is unless the build mode is always.

    let flag_request = requests
        .first()
        .filter(|request| request.source == Source::Flag);
    let prefer_request = flag_request.is_some() || (inputs.always && !requests.is_empty());

    if !prefer_request {
        if inputs.mode == Mode::Development
            && let Some(venv) = std::env::var_os("VIRTUAL_ENV").filter(|value| !value.is_empty())
        {
            let python = PathBuf::from(venv).join("bin").join("python");

            if python.is_file() {
                return Ok((python, Chosen::ActiveVirtualenv));
            }
        }

        let venv = inputs.project.join(".venv").join("bin").join("python");

        if venv.is_file() {
            return Ok((venv, Chosen::ProjectVirtualenv));
        }
    }

    if let Some(request) = requests.first() {
        let Some(uv) = inputs.uv else {
            return Err(ResolveError::NeedsUv {
                request: request.text.clone(),
                origin: request.source.as_str(),
            });
        };

        return match uv.python_find(&request.text, inputs.isolation)? {
            Some(python) => Ok((python, Chosen::Request(request.clone()))),
            None => Err(ResolveError::NotFound {
                request: request.text.clone(),
                origin: request.source.as_str(),
            }),
        };
    }

    if let Some(previous) = inputs.previous {
        return Ok((previous.to_path_buf(), Chosen::Previous));
    }

    find_on_path("python3")
        .map(|python| (python, Chosen::Default))
        .ok_or_else(|| ResolveError::NoPython {
            project: inputs.project.to_path_buf(),
        })
}

/// Resolve the directory, not the file: the interpreter is often a
/// symlink into a virtualenv, and following it would lose the venv.
fn canonical_dir(python: PathBuf) -> PathBuf {
    match (python.parent(), python.file_name()) {
        (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => match dir.canonicalize() {
            Ok(dir) => dir.join(name),
            Err(_) => python,
        },
        _ => python,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uv::tests::{EnvGuard, fake_uv};

    fn python3() -> Option<PathBuf> {
        find_on_path("python3")
    }

    fn inputs<'a>(project: &'a Path, uv: Option<&'a Uv>, isolation: &'a Isolation) -> Inputs<'a> {
        Inputs {
            mode: Mode::Daemon,
            flag: None,
            table: None,
            project,
            previous: None,
            always: false,
            uv,
            isolation,
        }
    }

    #[test]
    fn a_path_is_used_as_it_is_and_probed() {
        let Some(python3) = python3() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let isolation = Isolation::default();
        let text = python3.display().to_string();
        let mut inputs = inputs(dir.path(), None, &isolation);
        inputs.flag = Some(&text);
        let resolved = resolve(&inputs).unwrap();

        assert_eq!(resolved.chosen, Chosen::Path(Source::Flag));
        assert_eq!(resolved.probe.implementation, "cpython");
        assert!(resolved.describe().contains("from --python"));
    }

    #[test]
    fn a_request_goes_through_uv_and_a_missing_one_names_the_install() {
        let Some(python3) = python3() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let fake = fake_uv(dir.path(), Some(&python3.display().to_string()));
        let uv = Uv::find(Some(&fake), None).unwrap();
        let isolation = Isolation::default();
        std::fs::write(dir.path().join(".python-version"), "3.13\n").unwrap();
        let resolved = resolve(&inputs(dir.path(), Some(&uv), &isolation)).unwrap();

        assert_eq!(
            resolved.chosen,
            Chosen::Request(Request {
                text: "3.13".to_owned(),
                source: Source::VersionFile,
            })
        );
        assert!(resolved.describe().contains("3.13 from .python-version"));

        std::fs::create_dir_all(dir.path().join("none")).unwrap();
        let missing = fake_uv(&dir.path().join("none"), None);
        let uv = Uv::find(Some(&missing), None).unwrap();
        let error = resolve(&inputs(dir.path(), Some(&uv), &isolation)).unwrap_err();

        assert_eq!(
            error.to_string(),
            "no Python matching 3.13 (from .python-version) was found on this machine; `uv python \
             install 3.13` adds one"
        );
    }

    #[test]
    fn a_request_without_uv_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let isolation = Isolation::default();
        let mut inputs = inputs(dir.path(), None, &isolation);
        inputs.flag = Some("3.13");
        let error = resolve(&inputs).unwrap_err();

        assert!(matches!(error, ResolveError::NeedsUv { .. }));
        assert!(
            error
                .to_string()
                .starts_with("Python 3.13 (from --python) needs uv")
        );
    }

    #[test]
    fn requires_python_is_checked_against_requests_and_the_interpreter() {
        let Some(python3) = python3() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = 'x'\nrequires-python = '>=3.12'\n",
        )
        .unwrap();
        let isolation = Isolation::default();
        let mut inputs = inputs(dir.path(), None, &isolation);
        inputs.flag = Some("3.11");
        let error = resolve(&inputs).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Python 3.11 (from --python) is outside the project's requires-python >=3.12"
        );

        // The interpreter itself is checked too: a requirement no Python
        // meets fails naming the version found.

        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = 'x'\nrequires-python = '>=3.99'\n",
        )
        .unwrap();
        let text = python3.display().to_string();
        let fake = fake_uv(dir.path(), Some(&text));
        let uv = Uv::find(Some(&fake), None).unwrap();
        let mut by_path = self::tests::inputs(dir.path(), Some(&uv), &isolation);
        by_path.flag = Some(&text);
        let error = resolve(&by_path).unwrap_err();

        assert!(matches!(error, ResolveError::RequiresPython { .. }));
        assert!(
            error
                .to_string()
                .contains("but the project requires >=3.99")
        );
    }

    #[test]
    fn the_project_venv_wins_over_a_request_unless_always() {
        let Some(python3) = python3() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".venv").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&python3, bin.join("python")).unwrap();
        std::fs::write(dir.path().join(".python-version"), "3.13\n").unwrap();
        let fake = fake_uv(dir.path(), Some(&python3.display().to_string()));
        let uv = Uv::find(Some(&fake), None).unwrap();
        let isolation = Isolation::default();
        let _guard = EnvGuard::set("VIRTUAL_ENV", "");

        let resolved = resolve(&inputs(dir.path(), Some(&uv), &isolation)).unwrap();
        assert_eq!(resolved.chosen, Chosen::ProjectVirtualenv);
        assert!(resolved.python.ends_with(".venv/bin/python"));

        let mut always = inputs(dir.path(), Some(&uv), &isolation);
        always.always = true;
        let resolved = resolve(&always).unwrap();
        assert!(matches!(resolved.chosen, Chosen::Request(_)));

        // A request on the command line is answered whatever exists.

        let mut flagged = inputs(dir.path(), Some(&uv), &isolation);
        flagged.flag = Some("3.13");
        let resolved = resolve(&flagged).unwrap();
        assert_eq!(
            resolved.chosen,
            Chosen::Request(Request {
                text: "3.13".to_owned(),
                source: Source::Flag,
            })
        );
    }

    #[test]
    fn nothing_named_falls_back_to_the_previous_release_then_python3() {
        let Some(python3) = python3() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let isolation = Isolation::default();
        let _guard = EnvGuard::set("VIRTUAL_ENV", "");
        let mut inputs = inputs(dir.path(), None, &isolation);
        inputs.previous = Some(&python3);

        assert_eq!(resolve(&inputs).unwrap().chosen, Chosen::Previous);

        inputs.previous = None;
        assert_eq!(resolve(&inputs).unwrap().chosen, Chosen::Default);
    }
}
