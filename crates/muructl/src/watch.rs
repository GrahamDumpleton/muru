//! The file watcher behind `muructl run --reload` (design specification
//! section 9.8): it polls the watched directories and reports the files
//! that changed, respecting .gitignore and skipping caches, virtualenvs and
//! editor files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder};
use tokio::sync::mpsc;

/// Directory names never watched, besides what .gitignore excludes.
const SKIPPED_DIRECTORIES: [&str; 2] = ["__pycache__", "node_modules"];

/// Editor leftovers that never count as a change.
const EDITOR_FILES: [&str; 4] = ["*.swp", "*.swx", "*~", ".#*"];

/// What is watched and which files count.
pub struct Watch {
    directories: Vec<PathBuf>,
    include: GlobSet,
    exclude: GlobSet,
}

type Snapshot = HashMap<PathBuf, (SystemTime, u64)>;

impl Watch {
    /// Watch `directories` for files matching `include` but not `exclude`.
    pub fn new(
        directories: Vec<PathBuf>,
        include: &[String],
        exclude: &[String],
    ) -> anyhow::Result<Self> {
        Ok(Self {
            directories,
            include: globs(include)?,
            exclude: globs(&[exclude, &EDITOR_FILES.map(str::to_owned)].concat())?,
        })
    }

    /// The directories watched.
    pub fn directories(&self) -> &[PathBuf] {
        &self.directories
    }

    /// Poll every `interval` and send each file that changed, appeared or
    /// disappeared, until the receiver is dropped.
    pub async fn run(self, interval: Duration, changes: mpsc::Sender<PathBuf>) {
        let mut previous = self.snapshot();
        loop {
            tokio::time::sleep(interval).await;
            let current = self.snapshot();
            for path in changed(&previous, &current) {
                if changes.send(path).await.is_err() {
                    return;
                }
            }
            previous = current;
        }
    }

    fn snapshot(&self) -> Snapshot {
        let mut files = Snapshot::new();
        for directory in &self.directories {
            let walker = WalkBuilder::new(directory)
                .hidden(true)
                .git_ignore(true)
                .git_global(false)
                .git_exclude(true)
                .filter_entry(|entry| !skipped(entry))
                .build();
            for entry in walker.flatten() {
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let relative = entry.path().strip_prefix(directory).unwrap_or(entry.path());
                if !self.include.is_match(relative) || self.exclude.is_match(relative) {
                    continue;
                }
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.insert(entry.path().to_path_buf(), (modified, metadata.len()));
            }
        }
        files
    }
}

fn globs(patterns: &[String]) -> anyhow::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid pattern {pattern:?}"))?);
    }
    Ok(builder.build()?)
}

fn skipped(entry: &DirEntry) -> bool {
    if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    SKIPPED_DIRECTORIES.contains(&name.as_ref()) || is_virtualenv(entry.path())
}

fn is_virtualenv(directory: &Path) -> bool {
    directory.join("pyvenv.cfg").is_file()
}

/// Paths whose entry differs between two snapshots, sorted.
fn changed(previous: &Snapshot, current: &Snapshot) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = current
        .iter()
        .filter(|(path, stamp)| previous.get(*path) != Some(stamp))
        .map(|(path, _)| path.clone())
        .chain(
            previous
                .keys()
                .filter(|path| !current.contains_key(*path))
                .cloned(),
        )
        .collect();
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_and_excludes_apply_to_relative_paths() {
        let directory = std::env::temp_dir().join(format!("muru-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("pkg/__pycache__")).expect("mkdir");
        std::fs::create_dir_all(directory.join("venv")).expect("mkdir");
        std::fs::write(directory.join("app.py"), "x").expect("write");
        std::fs::write(directory.join("pkg/views.py"), "x").expect("write");
        std::fs::write(directory.join("pkg/__pycache__/views.pyc"), "x").expect("write");
        std::fs::write(directory.join("notes.txt"), "x").expect("write");
        std::fs::write(directory.join("app.py~"), "x").expect("write");
        std::fs::write(directory.join("venv/pyvenv.cfg"), "x").expect("write");
        std::fs::write(directory.join("venv/site.py"), "x").expect("write");
        let watch = Watch::new(
            vec![directory.clone()],
            &["**/*.py".to_owned()],
            &["pkg/skip*.py".to_owned()],
        )
        .expect("globs");
        let names: Vec<String> = {
            let mut names: Vec<String> = watch
                .snapshot()
                .keys()
                .map(|path| {
                    path.strip_prefix(&directory)
                        .expect("inside")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        };
        assert_eq!(names, ["app.py", "pkg/views.py"]);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn changes_are_additions_modifications_and_removals() {
        let stamp =
            |seconds: u64, size: u64| (SystemTime::UNIX_EPOCH + Duration::from_secs(seconds), size);
        let previous = Snapshot::from([
            (PathBuf::from("a.py"), stamp(1, 1)),
            (PathBuf::from("b.py"), stamp(1, 1)),
            (PathBuf::from("c.py"), stamp(1, 1)),
        ]);
        let current = Snapshot::from([
            (PathBuf::from("a.py"), stamp(1, 1)),
            (PathBuf::from("b.py"), stamp(2, 1)),
            (PathBuf::from("d.py"), stamp(1, 1)),
        ]);
        assert_eq!(
            changed(&previous, &current),
            [
                PathBuf::from("b.py"),
                PathBuf::from("c.py"),
                PathBuf::from("d.py")
            ]
        );
    }
}
