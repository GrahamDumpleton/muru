//! The `muructl` binary: CLI, subcommand dispatch and development mode.
//!
//! Phase 0 scope: `muructl run` starts a Python host that preloads one WSGI
//! application and forks workers, then serves HTTP in front of them
//! (design specification sections 3.1 and 6.1). Every command is defined
//! once with clap, which later generates help, man pages and the CLI
//! reference (section 14.4 and Appendix A).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

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
    Run(RunArgs),

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
                .block_on(run(args));
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

/// Development mode: one host with forked workers behind an embedded edge.
async fn run(args: RunArgs) -> anyhow::Result<()> {
    anyhow::ensure!(args.workers >= 1, "--workers must be at least 1");
    let run_dir = std::env::temp_dir().join(format!("muru-{}", std::process::id()));
    std::fs::create_dir_all(&run_dir).with_context(|| format!("creating {}", run_dir.display()))?;
    let python = python_interpreter(args.python);

    let mut command = tokio::process::Command::new(&python);
    command
        .arg("-m")
        .arg("muru.host")
        .arg("--app")
        .arg(&args.app)
        .arg("--socket-dir")
        .arg(&run_dir)
        .arg("--workers")
        .arg(args.workers.to_string())
        .arg("--threads")
        .arg(args.threads.to_string())
        .arg("--preload")
        .arg(args.preload.as_str())
        .stdin(Stdio::piped());
    if let Some(hooks) = &args.hooks {
        command.arg("--hooks").arg(hooks);
    }
    if args.link == Link::Tcp {
        command
            .arg("--tcp-base-port")
            .arg(args.tcp_base_port.to_string());
    }
    if let Some(dir) = &args.chdir {
        command.current_dir(dir);
    }
    let mut host = command
        .spawn()
        .with_context(|| format!("starting the host with {}", python.display()))?;
    // The host exits when this end of its stdin closes, whatever happens here.
    let host_stdin = host.stdin.take();

    let sockets: Vec<PathBuf> = (0..args.workers)
        .map(|index| match args.link {
            Link::Unix => run_dir.join(format!("worker-{index}.sock")),
            Link::Tcp => {
                let port = args.tcp_base_port + u16::try_from(index).unwrap_or(0);
                PathBuf::from(format!("127.0.0.1:{port}"))
            }
        })
        .collect();
    let started = Instant::now();
    loop {
        let ready = match args.link {
            Link::Unix => sockets.iter().all(|path| path.exists()),
            Link::Tcp => sockets.iter().all(|address| {
                std::net::TcpStream::connect(address.to_str().unwrap_or("")).is_ok()
            }),
        };
        if ready {
            break;
        }
        if let Some(status) = host.try_wait()? {
            anyhow::bail!("the host exited with {status} before its workers were ready");
        }
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(60),
            "the workers did not start within 60 seconds"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let edge = muru_edge::Edge::bind(muru_edge::Config {
        listen: args.bind,
        sockets,
        slots: args.threads,
        development: true,
    })
    .await?;
    println!(
        "muru: serving {} on http://{} with {} workers of {} threads",
        args.app,
        edge.local_addr(),
        args.workers,
        args.threads
    );

    let outcome = tokio::select! {
        result = edge.run() => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
        status = host.wait() => Err(anyhow::anyhow!("the host exited with {}", status?)),
    };

    // Stop the host: SIGTERM, and its stdin closes when we drop it.
    if let Some(pid) = host.id() {
        // SAFETY: kill(2) with a valid pid and signal has no memory-safety
        // preconditions; the pid belongs to the child we spawned.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    drop(host_stdin);
    if tokio::time::timeout(Duration::from_secs(10), host.wait())
        .await
        .is_err()
    {
        let _ = host.kill().await;
    }
    let _ = std::fs::remove_dir_all(&run_dir);
    outcome
}
