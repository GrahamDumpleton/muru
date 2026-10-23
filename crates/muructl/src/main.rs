//! The `muructl` binary: CLI, subcommand dispatch and development mode.
//!
//! Placeholder created with the project skeleton. Every command is defined
//! once with clap, which later generates help, man pages and the CLI
//! reference (design specification section 14.4 and Appendix A).

use clap::{Parser, Subcommand};

/// Routing edge, control plane and Python runtime for WSGI and ASGI applications.
#[derive(Parser)]
#[command(name = "muructl", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run an application with an embedded controller and edge (development mode).
    Run,

    /// Run or control the daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },

    /// Run an edge process (internal, started by the daemon).
    Edge {
        #[command(subcommand)]
        command: EdgeCommand,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Run the daemon in the foreground.
    Run,
}

#[derive(Subcommand)]
enum EdgeCommand {
    /// Run an edge process.
    Run,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Nothing is implemented yet; the skeleton only establishes the command shape.
    let name = match cli.command {
        Command::Run => "run",
        Command::Daemon {
            command: DaemonCommand::Run,
        } => "daemon run",
        Command::Edge {
            command: EdgeCommand::Run,
        } => "edge run",
    };
    anyhow::bail!("muructl {name} is not implemented yet; see docs/project/roadmap.md")
}
