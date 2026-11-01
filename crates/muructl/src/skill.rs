//! The skill a project carries for coding agents (design specification
//! section 10.11, ADR 0039): what the MCP tools cannot teach, embedded in
//! muructl and versioned with it, written into the project's skills
//! directory so it travels with the code and needs no channel of its own.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Subcommand;
use serde_json::json;

use crate::admin::{JsonArgs, print_answer};

/// The skill's text after its front matter.
const BODY: &str = include_str!("skill.md");

/// What the agent reads to decide when the skill applies.
const DESCRIPTION: &str = "Develop, deploy, test and debug this project's Python web application under Muru: releases, traffic, rollouts, events, logs and worker stacks, through the muru MCP tools or muructl --json.";

/// Claude Code's skills directory in a project, where `install` writes and
/// `status` looks unless told otherwise.
pub const DEFAULT_DIR: &str = ".claude/skills";

/// The skill subcommands.
#[derive(Subcommand)]
pub enum SkillCommand {
    /// Print the skill to standard output.
    Print,

    /// Write the skill into the project's skills directory, or update it
    /// there, as muru/SKILL.md.
    Install {
        /// The skills directory, for a client other than Claude Code.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_DIR)]
        dir: PathBuf,
    },

    /// Say whether the project's copy of the skill matches this muructl's;
    /// exits 1 when it is missing or differs.
    Status {
        /// The skills directory, for a client other than Claude Code.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_DIR)]
        dir: PathBuf,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// Run a `skill` subcommand.
pub fn run(command: SkillCommand) -> anyhow::Result<()> {
    match command {
        SkillCommand::Print => {
            print!("{}", text());
            Ok(())
        }
        SkillCommand::Install { dir } => {
            let path = file(&dir);
            let parent = path.parent().unwrap_or(&dir);
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            std::fs::write(&path, text()).with_context(|| format!("writing {}", path.display()))?;
            println!(
                "muru: wrote the skill for muructl {} to {}",
                version(),
                path.display()
            );
            Ok(())
        }
        SkillCommand::Status { dir, output } => {
            let report = status(&dir);
            let current = report["state"] == "current";

            if output.json {
                print_answer(&report)?;
            } else {
                println!("muru: {}", describe(&report));
            }

            if !current {
                std::process::exit(1);
            }

            Ok(())
        }
    }
}

/// The skill file in a skills directory.
pub fn file(dir: &Path) -> PathBuf {
    dir.join("muru").join("SKILL.md")
}

/// The version the skill is written for: muructl's own.
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The skill as written: front matter recording the version and a hash of
/// the body, so a copy can be checked against the muructl in use.
pub fn text() -> String {
    format!(
        "---\nname: muru\ndescription: {DESCRIPTION}\nmetadata:\n  muructl-version: \"{}\"\n  content-hash: \"sha256:{}\"\n---\n\n{BODY}",
        version(),
        hash(BODY)
    )
}

/// The body's SHA-256, in hex.
fn hash(body: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, body.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// How the project's copy compares with this muructl's skill: its path,
/// `current`, `missing` or `out_of_date`, and the versions.
pub fn status(dir: &Path) -> serde_json::Value {
    let path = file(dir);
    let installed = std::fs::read_to_string(&path).ok();
    let installed_version = installed.as_deref().and_then(|text| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix("muructl-version:"))
            .map(|value| value.trim().trim_matches('"').to_owned())
    });

    let state = match &installed {
        None => "missing",
        Some(text) if *text == self::text() => "current",
        Some(_) => "out_of_date",
    };

    json!({
        "path": path.display().to_string(),
        "state": state,
        "version": version(),
        "installed_version": installed_version,
    })
}

/// A status report as a sentence, naming what to run when the copy is
/// missing or out of date.
pub fn describe(report: &serde_json::Value) -> String {
    let path = report["path"].as_str().unwrap_or_default();

    match report["state"].as_str() {
        Some("current") => format!("the skill at {path} matches muructl {}", version()),
        Some("missing") => {
            format!("the project carries no skill at {path}; `muructl mcp skill install` writes it")
        }
        _ => format!(
            "the skill at {path} is from muructl {} and differs from muructl {}'s; `muructl mcp skill install` updates it",
            report["installed_version"].as_str().unwrap_or("unknown"),
            version()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{status, text};

    #[test]
    fn install_then_status_is_current_and_an_edit_is_out_of_date() {
        let dir = tempfile::tempdir().expect("a directory");
        assert_eq!(status(dir.path())["state"], "missing");

        let path = super::file(dir.path());
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, text()).expect("the skill");
        let report = status(dir.path());
        assert_eq!(report["state"], "current");
        assert_eq!(report["installed_version"], env!("CARGO_PKG_VERSION"));

        std::fs::write(&path, text().replace("Developing with Muru", "Edited")).expect("an edit");
        assert_eq!(status(dir.path())["state"], "out_of_date");
    }

    #[test]
    fn the_front_matter_names_the_skill_and_records_the_hash() {
        let text = text();
        assert!(text.starts_with("---\nname: muru\ndescription: "));
        assert!(text.contains("content-hash: \"sha256:"));
        assert!(text.contains("# Developing with Muru"));
    }
}
