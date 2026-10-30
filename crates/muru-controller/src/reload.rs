//! What `muructl daemon reload` reports (design specification section
//! 13.3): the tables of the daemon's file that changed and what was done
//! about each, those that did not, what could not be applied, and the
//! tables read only when the daemon starts.

use std::path::{Path, PathBuf};

/// The tables of the daemon's file read only when the daemon starts, as
/// the reload names them. The daemon guide lists the same tables, and an
/// integration test holds the two to each other.
pub const READ_AT_START: &[&str] = &[
    "[metrics]",
    "[dashboard] listen",
    "[overlay_webhook]",
    "[daemon] user",
];

/// A reload's outcome, table by table, rendered as the command's message
/// and the `config.reloaded` event.
#[derive(Debug, Default)]
pub struct Report {
    path: PathBuf,
    changed: Vec<String>,
    unchanged: Vec<String>,
    failed: Vec<String>,
    kept: Vec<String>,
}

impl Report {
    /// A report on reloading the file at `path`.
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            ..Self::default()
        }
    }

    /// A table changed, and `what` was done about it.
    pub fn changed(&mut self, table: &str, what: &str) {
        self.changed.push(format!("{table} ({what})"));
    }

    /// A table is as it was.
    pub fn unchanged(&mut self, table: &str) {
        self.unchanged.push(table.to_owned());
    }

    /// A table changed but the change could not be applied, which makes
    /// the reload fail once everything else is applied.
    pub fn failed(&mut self, table: &str, error: &str) {
        self.failed.push(format!("{table}: {error}"));
    }

    /// Something stays as it was because its replacement was refused:
    /// a host's certificate, say. Said, but no failure of the reload.
    pub fn kept(&mut self, what: &str, why: &str) {
        self.kept.push(format!("{what} ({why})"));
    }

    /// Whether every change was applied.
    pub fn is_ok(&self) -> bool {
        self.failed.is_empty()
    }

    /// The report as one sentence.
    pub fn message(&self) -> String {
        let mut parts = Vec::new();

        if !self.changed.is_empty() {
            parts.push(format!("changed {}", named_list(&self.changed)));
        }

        if !self.unchanged.is_empty() {
            let how = if self.changed.is_empty() {
                "nothing changed among"
            } else {
                "unchanged"
            };
            parts.push(format!("{how} {}", named_list(&self.unchanged)));
        }

        for failure in &self.failed {
            parts.push(format!("could not apply {failure}"));
        }

        parts.extend(self.kept.iter().cloned());

        parts.push(format!("{} are read at start", named_list(READ_AT_START)));
        format!("reloaded {}: {}", self.path.display(), parts.join("; "))
    }

    /// The message, as the command's answer: an error when a change
    /// could not be applied.
    pub fn outcome(self) -> Result<String, String> {
        if self.is_ok() {
            Ok(self.message())
        } else {
            Err(self.message())
        }
    }
}

/// Items as a sentence names them: "a, b and c".
fn named_list<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [only] => only.as_ref().to_owned(),
        [rest @ .., last] => format!(
            "{} and {}",
            rest.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_changed_says_so() {
        let mut report = Report::new(Path::new("/etc/muru/muru.toml"));
        report.unchanged("[proxy]");
        report.unchanged("[limits]");
        let message = report.message();
        assert!(message.starts_with(
            "reloaded /etc/muru/muru.toml: nothing changed among [proxy] and [limits]; "
        ));
        assert!(message.ends_with(" are read at start"));
        assert!(message.contains("[metrics]"));
    }

    #[test]
    fn changes_come_first_with_what_was_done() {
        let mut report = Report::new(Path::new("muru.toml"));
        report.unchanged("[proxy]");
        report.changed("[limits]", "the edge has it");
        report.changed(
            "[[listener]]",
            "https://[::]:8443 added; the edge was replaced",
        );
        assert_eq!(
            report.message(),
            format!(
                "reloaded muru.toml: changed [limits] (the edge has it) and [[listener]] (https://[::]:8443 added; the edge was replaced); unchanged [proxy]; {} are read at start",
                named_list(READ_AT_START)
            )
        );
        assert!(report.outcome().is_ok());
    }

    #[test]
    fn what_is_kept_is_said_without_failing_the_reload() {
        let mut report = Report::new(Path::new("muru.toml"));
        report.unchanged("[proxy]");
        report.kept(
            "host shop keeps its certificate",
            "the certificate and private key do not go together",
        );
        assert!(report.is_ok());
        assert!(report.message().contains(
            "nothing changed among [proxy]; host shop keeps its certificate (the certificate and private key do not go together); "
        ));
    }

    #[test]
    fn a_change_that_could_not_be_applied_fails_the_reload() {
        let mut report = Report::new(Path::new("muru.toml"));
        report.changed("[proxy]", "the edge has it");
        report.failed("[[listener]]", "cannot listen on 0.0.0.0:443: in use");
        assert!(!report.is_ok());
        let message = report.outcome().unwrap_err();
        assert!(message.contains("changed [proxy] (the edge has it); could not apply [[listener]]: cannot listen on 0.0.0.0:443: in use;"));
    }
}
