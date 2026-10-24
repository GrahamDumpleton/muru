//! The `muructl` binary: CLI, subcommand dispatch and development mode.
//!
//! `muructl run` starts a Python host that preloads one WSGI application
//! and forks workers, then serves HTTP in front of them (design
//! specification sections 6.1 and 13.1). With --reload, a changed source
//! file starts a new release behind the same edge (section 9.8). Every
//! command is defined once with clap, which later generates help, man
//! pages and the CLI reference (section 14.4 and Appendix A).

mod host;
mod watch;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use tokio::sync::mpsc;

use crate::host::{HostProcess, HostSpec};
use crate::watch::Watch;

/// How long a host may take to get its workers listening.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the watcher looks for changes.
const WATCH_INTERVAL: Duration = Duration::from_millis(500);
/// How long changes must have stopped before a reload starts.
const DEBOUNCE: Duration = Duration::from_millis(300);

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
    Run(Box<RunArgs>),

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

#[derive(Args)]
struct RunArgs {
    /// The application as module:callable, for example app:application.
    app: String,

    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// Worker processes forked from the preloaded host.
    #[arg(long, default_value_t = 2)]
    workers: usize,

    /// Application threads per worker.
    #[arg(long, default_value_t = 4)]
    threads: usize,

    /// Import the application in the host before forking: auto preloads
    /// when the import is fork-safe and imports per worker otherwise, true
    /// fails instead, false imports in each worker.
    #[arg(long, default_value = "auto")]
    preload: Preload,

    /// Module providing the lifecycle hooks: on_load, pre_fork, post_fork,
    /// on_ready, on_drain, on_exit and on_reopen_logs.
    #[arg(long)]
    hooks: Option<String>,

    /// Where access log lines go: terminal, off, errors (4xx and 5xx
    /// responses and requests slower than --slow) or a file path, which
    /// receives JSON lines.
    #[arg(long, default_value = "terminal")]
    access_log: String,

    /// Hide requests whose path matches the pattern from the access log
    /// (`*` matches anything, `?` one character). Repeatable.
    #[arg(long = "access-log-exclude", value_name = "PATTERN")]
    access_log_exclude: Vec<String>,

    /// Requests at least this slow count as errors for --access-log errors,
    /// for example 500ms or 2s.
    #[arg(long, default_value = "1s", value_parser = parse_duration)]
    slow: Duration,

    /// Start a new release when a watched source file changes.
    #[arg(long)]
    reload: bool,

    /// Directory to watch for changes; defaults to --chdir or the current
    /// directory. Repeatable.
    #[arg(long = "reload-dir", value_name = "DIR")]
    reload_dir: Vec<PathBuf>,

    /// Files that count as source, relative to a watched directory.
    /// Repeatable; the default is `**/*.py`.
    #[arg(long = "reload-include", value_name = "GLOB")]
    reload_include: Vec<String>,

    /// Files that never trigger a reload. Repeatable.
    #[arg(long = "reload-exclude", value_name = "GLOB")]
    reload_exclude: Vec<String>,

    /// Python interpreter with the muru runtime library installed.
    /// Defaults to the active virtualenv's interpreter, else python3 on PATH.
    #[arg(long)]
    python: Option<PathBuf>,

    /// Directory to run the host in, added to the module search path.
    #[arg(long)]
    chdir: Option<PathBuf>,

    /// Edge-to-worker link: unix (sockets in a run directory) or tcp
    /// (loopback ports from --tcp-base-port), the latter for experiments.
    #[arg(long, default_value = "unix")]
    link: Link,

    /// First loopback port for the tcp link; each worker takes the next one.
    #[arg(long, default_value_t = 9100)]
    tcp_base_port: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Link {
    Unix,
    Tcp,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Preload {
    Auto,
    True,
    False,
}

impl Preload {
    fn as_str(self) -> &'static str {
        match self {
            Preload::Auto => "auto",
            Preload::True => "true",
            Preload::False => "false",
        }
    }
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MURU_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    let name = match cli.command {
        Command::Run(args) => {
            return tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(run(*args));
        }
        Command::Daemon {
            command: DaemonCommand::Run,
        } => "daemon run",
        Command::Edge {
            command: EdgeCommand::Run,
        } => "edge run",
    };
    anyhow::bail!("muructl {name} is not implemented yet; see docs/project/roadmap.md")
}

/// Parse a duration such as 250ms, 1.5s or 2 (seconds).
fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (number, scale) = if let Some(number) = text.strip_suffix("ms") {
        (number, 0.001)
    } else if let Some(number) = text.strip_suffix('s') {
        (number, 1.0)
    } else {
        (text, 1.0)
    };
    let value: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("{text:?} is not a duration such as 500ms or 2s"))?;
    if value < 0.0 {
        return Err("a duration cannot be negative".to_owned());
    }
    Ok(Duration::from_secs_f64(value * scale))
}

fn access_log_config(args: &RunArgs) -> muru_edge::access::Config {
    use muru_edge::access::Destination;

    let destination = match args.access_log.as_str() {
        "terminal" => Destination::Terminal,
        "off" => Destination::Off,
        "errors" => Destination::Errors,
        path => Destination::File(PathBuf::from(path)),
    };
    muru_edge::access::Config {
        destination,
        slow: args.slow,
        exclude: args.access_log_exclude.clone(),
    }
}

/// The interpreter to run the host with. A path is made absolute, because a
/// virtualenv's interpreter locates its site-packages from its own path.
fn python_interpreter(explicit: Option<PathBuf>) -> PathBuf {
    let python = explicit
        .or_else(|| {
            std::env::var_os("VIRTUAL_ENV")
                .map(|venv| PathBuf::from(venv).join("bin").join("python"))
        })
        .unwrap_or_else(|| PathBuf::from("python3"));
    match (python.parent(), python.file_name()) {
        (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => {
            // Resolve the directory, not the file: the interpreter is often a
            // symlink into a virtualenv, and following it would lose the venv.
            match dir.canonicalize() {
                Ok(dir) => dir.join(name),
                Err(_) => python,
            }
        }
        _ => python,
    }
}

/// Development mode: one host with forked workers behind an embedded edge,
/// replaced by a new one when --reload sees a source change.
async fn run(args: RunArgs) -> anyhow::Result<()> {
    anyhow::ensure!(args.workers >= 1, "--workers must be at least 1");
    let run_dir = std::env::temp_dir().join(format!("muru-{}", std::process::id()));
    std::fs::create_dir_all(&run_dir).with_context(|| format!("creating {}", run_dir.display()))?;
    let access_log = access_log_config(&args);
    let spec = HostSpec {
        python: python_interpreter(args.python.clone()),
        app: args.app.clone(),
        workers: args.workers,
        threads: args.threads,
        preload: args.preload.as_str(),
        hooks: args.hooks.clone(),
        chdir: args.chdir.clone(),
        tcp_base_port: (args.link == Link::Tcp).then_some(args.tcp_base_port),
    };

    let mut current = HostProcess::start(&spec, &run_dir, 1)?;
    if let Err(error) = current.wait_ready(START_TIMEOUT).await {
        current.stop().await;
        let _ = std::fs::remove_dir_all(&run_dir);
        return Err(error);
    }

    let edge = muru_edge::Edge::bind(muru_edge::Config {
        listen: args.bind,
        sockets: current.sockets.clone(),
        slots: args.threads,
        development: true,
        access_log,
    })
    .await?;
    println!(
        "muru: serving {} on http://{} with {} workers of {} threads",
        args.app,
        edge.local_addr(),
        args.workers,
        args.threads
    );

    let (changed_tx, mut changed) = mpsc::channel::<PathBuf>(64);
    if args.reload {
        let directories = if args.reload_dir.is_empty() {
            vec![args.chdir.clone().unwrap_or_else(|| PathBuf::from("."))]
        } else {
            args.reload_dir.clone()
        };
        let directories: Vec<PathBuf> = directories
            .into_iter()
            .map(|directory| directory.canonicalize().unwrap_or(directory))
            .collect();
        let include = if args.reload_include.is_empty() {
            vec!["**/*.py".to_owned()]
        } else {
            args.reload_include.clone()
        };
        let watch = Watch::new(directories, &include, &args.reload_exclude)?;
        println!(
            "muru: watching {} for changes",
            watch
                .directories()
                .iter()
                .map(|directory| directory.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        tokio::spawn(watch.run(WATCH_INTERVAL, changed_tx));
    } else {
        drop(changed_tx);
    }

    let pools = edge.pools();
    let mut serving = tokio::spawn(edge.run());
    let mut release = 1;
    let outcome = loop {
        tokio::select! {
            served = &mut serving => {
                break served.map_err(anyhow::Error::from).and_then(|result| result);
            }
            _ = tokio::signal::ctrl_c() => break Ok(()),
            status = current.exited() => {
                break Err(anyhow::anyhow!("the host exited with {}", status?));
            }
            Some(path) = changed.recv() => {
                let mut paths = vec![path];
                settle(&mut changed, &mut paths).await;
                println!("muru: reloading, {} changed", describe(&paths));
                loop {
                    release += 1;
                    let mut next = match HostProcess::start(&spec, &run_dir, release) {
                        Ok(next) => next,
                        Err(error) => {
                            println!("muru: reload failed: {error:#}; release r{} keeps serving", current.release);
                            break;
                        }
                    };
                    tokio::select! {
                        ready = next.wait_ready(START_TIMEOUT) => match ready {
                            Ok(()) => {
                                match muru_edge::link::LinkPool::new(&next.sockets, args.threads) {
                                    Ok(pool) => pools.replace(pool),
                                    Err(error) => {
                                        println!("muru: reload failed: {error:#}; release r{} keeps serving", current.release);
                                        next.stop().await;
                                        break;
                                    }
                                }
                                println!("muru: switched to release r{release}");
                                let previous = std::mem::replace(&mut current, next);
                                tokio::spawn(previous.stop());
                                break;
                            }
                            Err(error) => {
                                println!("muru: reload failed: {error:#}; release r{} keeps serving", current.release);
                                next.stop().await;
                                break;
                            }
                        },
                        Some(path) = changed.recv() => {
                            // Section 9.8: a release still starting is abandoned
                            // when more changes arrive.
                            let mut paths = vec![path];
                            settle(&mut changed, &mut paths).await;
                            println!("muru: {} changed again, restarting the reload", describe(&paths));
                            next.stop().await;
                        }
                    }
                }
            }
        }
    };

    current.stop().await;
    let _ = std::fs::remove_dir_all(&run_dir);
    outcome
}

/// Keep collecting changes until none has arrived for the debounce period.
async fn settle(changed: &mut mpsc::Receiver<PathBuf>, paths: &mut Vec<PathBuf>) {
    while let Ok(Some(path)) = tokio::time::timeout(DEBOUNCE, changed.recv()).await {
        paths.push(path);
    }
}

/// Name the changed files briefly: the first, and how many more.
fn describe(paths: &[PathBuf]) -> String {
    let first = paths
        .first()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    match paths.len() {
        0 | 1 => first,
        more => format!("{first} and {} more", more - 1),
    }
}
