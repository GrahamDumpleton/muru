//! The `muructl` binary: CLI, subcommand dispatch and development mode.
//!
//! `muructl run` starts a Python host that preloads one WSGI application
//! and forks workers, then serves HTTP in front of them (design
//! specification sections 6.1 and 13.1). With --reload, a changed source
//! file starts a new release behind the same edge (section 9.8). Every
//! command is defined once with clap, which later generates help, man
//! pages and the CLI reference (section 14.4 and Appendix A).

mod admin;
mod dashboard;
mod devcert;
mod edge;
mod pstats;
mod watch;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use tokio::sync::mpsc;

use crate::watch::Watch;
use muru_controller::host::{HostProcess, HostSpec};
use muru_controller::logs::Logs;

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
        command: admin::DaemonCommand,
    },

    /// Make sure a daemon is running for this user.
    Up(admin::DaemonRunArgs),

    /// Create a release from a source directory and move traffic to it.
    Deploy(Box<admin::DeployArgs>),

    /// Manage applications.
    App {
        #[command(subcommand)]
        command: admin::AppCommand,
    },

    /// Manage releases.
    Release {
        #[command(subcommand)]
        command: admin::ReleaseCommand,
    },

    /// Manage virtual hosts.
    Host {
        #[command(subcommand)]
        command: admin::HostCommand,
    },

    /// Manage routes.
    Route {
        #[command(subcommand)]
        command: admin::RouteCommand,
    },

    /// Manage HTTP upstreams.
    Upstream {
        #[command(subcommand)]
        command: admin::UpstreamCommand,
    },

    /// Restore traffic to an application's previous release.
    Rollback {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: admin::SocketArgs,
    },

    /// Take a release out of traffic and stop it.
    Drain {
        /// The release, as app@release.
        release: String,

        #[command(flatten)]
        socket: admin::SocketArgs,
    },

    /// Run and control rollouts: canaries with gates, and blue/green.
    Rollout {
        #[command(subcommand)]
        command: admin::RolloutCommand,
    },

    /// Pin a worker group's count, or show the autoscaler's state.
    Scale(admin::ScaleArgs),

    /// Overlays: list them, activate or end one by hand, preview what
    /// one changes, or make one's changes permanent (section 9.10).
    Overlay {
        #[command(subcommand)]
        command: admin::OverlayCommand,
    },

    /// Serve an application's maintenance page at the edge, or serve it
    /// as usual again.
    Maintenance {
        #[command(subcommand)]
        command: admin::MaintenanceCommand,
    },

    /// The daemon's dashboard.
    Dashboard {
        #[command(subcommand)]
        command: admin::DashboardCommand,
    },

    /// Show the daemon's status.
    Status {
        #[command(flatten)]
        socket: admin::SocketArgs,
    },

    /// Apply declarative configuration from a file or a directory of files.
    Apply(Box<admin::ApplyArgs>),

    /// Export, compare or inspect declarative configuration.
    Config {
        #[command(subcommand)]
        command: admin::ConfigCommand,
    },

    /// Manage the file secret stores (section 10.8).
    Secret {
        #[command(subcommand)]
        command: admin::SecretCommand,
    },

    /// Look into a release's workers: status, stacks, tasks, inspect.
    Debug {
        #[command(subcommand)]
        command: admin::DebugCommand,
    },

    /// Change logger levels in a release's workers for a period.
    Logging {
        #[command(subcommand)]
        command: admin::LoggingCommand,
    },

    /// Show or follow the daemon's events.
    Events {
        /// Keep printing events as they happen.
        #[arg(long, short)]
        follow: bool,

        /// Start after this sequence number.
        #[arg(long, default_value = "0")]
        after: u64,

        #[command(flatten)]
        socket: admin::SocketArgs,
    },

    /// Show or follow a release's output or the access log, or reopen
    /// the log files after an external rotation.
    Logs(admin::LogsArgs),

    /// Print the node's metrics as Prometheus text.
    Metrics {
        #[command(flatten)]
        socket: admin::SocketArgs,
    },

    /// Serve a directory of files with no configuration (section 4.9),
    /// for a built front end or a documentation build.
    ServeStatic(Box<ServeStaticArgs>),

    /// Run an edge process (internal, started by the daemon).
    Edge {
        #[command(subcommand)]
        command: edge::EdgeCommand,
    },
}

/// What `serve-static` takes.
#[derive(Args)]
struct ServeStaticArgs {
    /// The directory to serve.
    directory: PathBuf,

    /// Address to listen on, as IP or IP:PORT.
    #[arg(long, default_value = "127.0.0.1", value_parser = parse_bind)]
    bind: Bind,

    /// Port for HTTP (default 8080), or auto to pick a free port and
    /// print it.
    #[arg(long, value_parser = parse_port)]
    port: Option<Port>,

    /// A file served for a directory, tried in order (default index.html).
    /// Repeatable.
    #[arg(long = "index", value_name = "NAME")]
    index: Vec<String>,

    /// The file served for any path that has no file of its own, for a
    /// single-page application.
    #[arg(long, value_name = "NAME")]
    fallback: Option<String>,

    /// Do not compress responses or serve precompressed variants.
    #[arg(long)]
    no_compression: bool,

    /// Where access log lines go: terminal, off, errors or a file path.
    #[arg(long, default_value = "terminal")]
    access_log: String,
}

/// Serve a directory with the edge's static file code and nothing else.
async fn serve_static(args: ServeStaticArgs) -> anyhow::Result<()> {
    let directory = args
        .directory
        .canonicalize()
        .with_context(|| format!("{} is not a directory", args.directory.display()))?;
    anyhow::ensure!(
        directory.is_dir(),
        "{} is not a directory",
        directory.display()
    );

    let port = match args.port {
        Some(Port::Auto) => 0,
        Some(Port::Number(port)) => port,
        None => args.bind.port.unwrap_or(8080),
    };

    let compression = if args.no_compression {
        muru_edge::compression::Compression::off()
    } else {
        muru_edge::compression::Compression::default()
    };
    let access_log = muru_edge::access::Config {
        destination: match args.access_log.as_str() {
            "terminal" => muru_edge::access::Destination::Terminal,
            "off" => muru_edge::access::Destination::Off,
            "errors" => muru_edge::access::Destination::Errors,
            path => muru_edge::access::Destination::File(PathBuf::from(path)),
        },
        ..muru_edge::access::Config::default()
    };
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("installing the SIGINT handler")?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;
    let edge = muru_edge::Edge::bind(muru_edge::Config {
        listen: SocketAddr::new(args.bind.ip, port),
        app: "static".to_owned(),
        release: String::new(),
        sockets: Vec::new(),
        groups: Vec::new(),
        mounts: Vec::new(),
        accel_redirect: false,
        cgi_local_redirect: false,
        sendfile: Arc::default(),
        grpc_web_origins: Vec::new(),
        errors: Arc::new(muru_edge::errors::ErrorPolicy::default()),
        development: true,
        access_log,
        limits: muru_edge::Limits::default(),
        compression: compression.clone(),
        https: None,
        spool: None,
    })
    .await
    .map_err(|error| explain_bind_error(error, SocketAddr::new(args.bind.ip, 0)))?;

    // One default host with one static route at /, as a snapshot would
    // describe it, so the same code serves here and under the daemon.

    let snapshot = muru_proto::edge::v1::Snapshot {
        version: 1,
        hosts: vec![muru_proto::edge::v1::Host {
            name: "static".to_owned(),
            default: true,
            ..Default::default()
        }],
        routes: vec![muru_proto::edge::v1::Route {
            host: "static".to_owned(),
            path: "/".to_owned(),
            static_dir: directory.display().to_string(),
            index: args.index.clone(),
            fallback_file: args.fallback.clone().unwrap_or_default(),
            ..Default::default()
        }],
        compression: Some(compression.to_message()),
        ..Default::default()
    };
    let routing = muru_edge::routing::Routing::from_snapshot(&snapshot).await?;
    edge.pools().apply(routing);
    println!(
        "muru: serving {} on http://{}",
        directory.display(),
        edge.local_addr()
    );
    let mut serving = tokio::spawn(edge.run());

    tokio::select! {
        served = &mut serving => served.map_err(anyhow::Error::from).and_then(|result| result),
        _ = interrupt.recv() => Ok(()),
        _ = terminate.recv() => Ok(()),
    }
}

#[derive(Args)]
struct RunArgs {
    /// The application as module:callable, or a project directory (or its
    /// pyproject.toml or muru.toml) whose [tool.muru] table names it.
    /// Without it, the project in --chdir or the current directory is used.
    #[arg(value_name = "ENTRYPOINT or PATH")]
    app: Option<String>,

    /// The interface the entrypoint speaks: auto (detected from the
    /// callable), wsgi, asgi, or grpc for a function that registers gRPC
    /// services.
    #[arg(long, value_name = "NAME")]
    interface: Option<muru_config::Interface>,

    /// The entrypoint is a factory, called in each worker after post_fork
    /// to get the application.
    #[arg(long)]
    factory: bool,

    /// Address to listen on, as IP or IP:PORT.
    #[arg(long, default_value = "127.0.0.1", value_parser = parse_bind)]
    bind: Bind,

    /// Port for HTTP (default 8080), or auto to pick free ports and print
    /// them.
    #[arg(long, value_parser = parse_port)]
    port: Option<Port>,

    /// Worker processes forked from the preloaded host (default 1).
    #[arg(long)]
    processes: Option<usize>,

    /// Application threads per worker (default 4).
    #[arg(long)]
    threads: Option<usize>,

    /// Threads for synchronous code called from an ASGI application
    /// through sync_to_async or asyncio.to_thread (default 8).
    #[arg(long, value_name = "N")]
    sync_threads: Option<usize>,

    /// Requests an ASGI worker's event loop takes at once (default 1000).
    #[arg(long, value_name = "N")]
    concurrency: Option<usize>,

    /// Import the application in the host before forking: auto preloads
    /// when the import is fork-safe and imports per worker otherwise, true
    /// fails instead, false imports in each worker (default auto).
    #[arg(long, value_name = "MODE")]
    preload: Option<muru_config::Preload>,

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
    #[arg(long, default_value = "1s", value_parser = muru_config::parse_duration)]
    slow: Duration,

    /// Largest request body accepted, for example 100MB or 512KB (KB, MB
    /// and GB are powers of 1024). Development runs default to 100MB, and
    /// 10MB with --allow-public.
    #[arg(long, value_name = "SIZE", value_parser = muru_config::parse_size)]
    max_body_size: Option<u64>,

    /// Override a timeout as NAME=DURATION: queue (5s), deadline (300s),
    /// header_read (30s), body_idle (60s), response_idle (60s),
    /// keep_alive (20s) or keep_alive_proxy (75s) at the edge; soft
    /// (off), hard (off), hard_drain_grace (10s), shutdown (5s) or
    /// loop_stall (10s) in the host, which interrupts a request that runs
    /// past the soft timeout and replaces a worker whose request runs
    /// past the hard timeout, or whose event loop stalls past
    /// loop_stall. Repeatable.
    #[arg(long = "timeout", value_name = "NAME=DURATION", value_parser = parse_timeout)]
    timeouts: Vec<(String, Duration)>,

    /// Open connections the listener holds at once (default 65536);
    /// beyond it clients get a fast 503 (section 4.18).
    #[arg(long, value_name = "N")]
    max_connections: Option<usize>,

    /// Open connections one client address may hold (default 256; 0 for
    /// no limit).
    #[arg(long, value_name = "N")]
    max_connections_per_client: Option<usize>,

    /// Requests served on one connection before it is closed (default 0,
    /// no limit).
    #[arg(long, value_name = "N")]
    max_requests_per_connection: Option<u64>,

    /// Replace a worker after this many requests, with jitter.
    #[arg(long, value_name = "N")]
    max_requests: Option<u64>,

    /// Replace a worker after this long, for example 1h, with jitter.
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    max_lifetime: Option<Duration>,

    /// A memory setting as KEY=VALUE: measure (uss, pss or rss),
    /// soft_limit (for example 800MB), soft_for (2m), reclaim_first
    /// (true), hard_limit (1200MB) or check_interval (5s). Repeatable.
    #[arg(long = "memory", value_name = "KEY=VALUE", value_parser = parse_memory_setting)]
    memory: Vec<(String, String)>,

    /// Address for the read-only dashboard on loopback, or off. On at
    /// 127.0.0.1:8081 by default, off with --allow-public unless given.
    #[arg(long, value_name = "ADDRESS")]
    dashboard: Option<String>,

    /// Do not compress responses (section 4.17); they are compressed for
    /// clients that accept br, zstd or gzip otherwise.
    #[arg(long)]
    no_compression: bool,

    /// Serve an address other than loopback. Development behaviours are
    /// then off (no tracebacks in error pages, the 10MB body limit, no
    /// dashboard unless asked for), but this is still not the supported
    /// way to run in production, which is the daemon's job.
    #[arg(long)]
    allow_public: bool,

    /// Serve HTTPS as well, on --https-port, with a development certificate
    /// unless --tls-cert is given.
    #[arg(long)]
    https: bool,

    /// Serve HTTPS only: plain HTTP requests are redirected to it.
    #[arg(long)]
    https_only: bool,

    /// Port for HTTPS, on the address of --bind (default 8443, or auto
    /// when --port is auto).
    #[arg(long, value_parser = parse_port)]
    https_port: Option<Port>,

    /// Certificate chain to serve, PEM, instead of a development
    /// certificate. Implies --https.
    #[arg(long, value_name = "FILE", requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// Private key for --tls-cert, PEM.
    #[arg(long, value_name = "FILE", requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// Extra name for the development certificate, besides localhost,
    /// 127.0.0.1 and ::1. Repeatable.
    #[arg(long = "host", value_name = "NAME")]
    hosts: Vec<String>,

    /// Write the equivalent declarative configuration (appendix C) to
    /// standard output instead of running: the path from a development
    /// run to a long-running setup.
    #[arg(long)]
    print_config: bool,

    /// One process and one thread, serving without forking, for debuggers.
    /// Implies --processes 1 --threads 1.
    #[arg(long)]
    single: bool,

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
    /// Defaults to the active virtualenv's interpreter, else the project's
    /// .venv, else python3 on PATH.
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

/// The --bind value: an address with or without a port.
#[derive(Debug, Clone, Copy)]
struct Bind {
    ip: IpAddr,
    port: Option<u16>,
}

fn parse_bind(text: &str) -> Result<Bind, String> {
    if let Ok(address) = text.parse::<SocketAddr>() {
        return Ok(Bind {
            ip: address.ip(),
            port: Some(address.port()),
        });
    }

    text.parse::<IpAddr>()
        .map(|ip| Bind { ip, port: None })
        .map_err(|_| format!("{text:?} is not an address such as 127.0.0.1 or 127.0.0.1:9000"))
}

/// A port setting: a number, or auto for a free one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Port {
    Auto,
    Number(u16),
}

fn parse_port(text: &str) -> Result<Port, String> {
    if text.eq_ignore_ascii_case("auto") {
        return Ok(Port::Auto);
    }

    text.parse::<u16>()
        .map(Port::Number)
        .map_err(|_| format!("{text:?} is not a port number or auto"))
}

/// The HTTP and HTTPS addresses to bind (section 13.1): --port wins over a
/// port in --bind, then 8080; --https-port, else auto when --port is auto,
/// else 8443. Port 0 asks the system for a free one.
fn addresses(args: &RunArgs) -> (SocketAddr, SocketAddr) {
    const HTTP: u16 = 8080;
    const HTTPS: u16 = 8443;

    let http = match args.port {
        Some(Port::Auto) => 0,
        Some(Port::Number(port)) => port,
        None => args.bind.port.unwrap_or(HTTP),
    };

    let https = match args.https_port {
        Some(Port::Auto) => 0,
        Some(Port::Number(port)) => port,
        None if args.port == Some(Port::Auto) => 0,
        None => HTTPS,
    };

    (
        SocketAddr::new(args.bind.ip, http),
        SocketAddr::new(args.bind.ip, https),
    )
}

/// Turn a failure to bind into advice: which port, who holds it when that
/// can be found out, and what to do.
fn explain_bind_error(error: anyhow::Error, https: SocketAddr) -> anyhow::Error {
    let Some(bind) = error.downcast_ref::<muru_edge::BindError>() else {
        return error;
    };

    if bind.source.kind() != std::io::ErrorKind::AddrInUse {
        return error;
    }

    let (address, port) = (bind.address, bind.address.port());

    let flag = if address == https {
        "--https-port"
    } else {
        "--port"
    };

    match port_holder(port) {
        Some(holder) => anyhow::anyhow!(
            "port {port} on {} is in use by {holder}; choose another with {flag}, or stop it",
            address.ip()
        ),
        None => anyhow::anyhow!(
            "port {port} on {} is in use; choose another with {flag} ({flag} auto picks a free one)",
            address.ip()
        ),
    }
}

/// The process listening on a TCP port, as "command (pid N)", found with
/// lsof or ss when either is installed.
fn port_holder(port: u16) -> Option<String> {
    let lsof = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
        .output()
        .ok()
        .filter(|output| output.status.success());

    if let Some(output) = lsof {
        let text = String::from_utf8_lossy(&output.stdout);
        let pid = text.lines().find_map(|line| line.strip_prefix('p'));
        let command = text.lines().find_map(|line| line.strip_prefix('c'));

        if let (Some(pid), Some(command)) = (pid, command) {
            return Some(format!("{command} (pid {pid})"));
        }
    }

    let ss = std::process::Command::new("ss")
        .args(["-lptnH", &format!("sport = :{port}")])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let text = String::from_utf8_lossy(&ss.stdout);

    // users:(("python3",pid=1234,fd=5))

    let users = text.split("users:((\"").nth(1)?;
    let (command, rest) = users.split_once('"')?;
    let pid = rest.split("pid=").nth(1)?.split(',').next()?;
    Some(format!("{command} (pid {pid})"))
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Link {
    Unix,
    Tcp,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MURU_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match cli.command {
        Command::Run(args) => runtime.block_on(run(*args)),
        Command::ServeStatic(args) => runtime.block_on(serve_static(*args)),
        Command::Daemon { command } => runtime.block_on(async move {
            match command {
                admin::DaemonCommand::Run(args) => admin::daemon_run(args).await,
                admin::DaemonCommand::Start(args) => admin::daemon_start(args).await,
                admin::DaemonCommand::Stop {
                    shutdown,
                    timeout,
                    socket,
                } => admin::daemon_stop(socket, shutdown, timeout).await,
                admin::DaemonCommand::Reload { socket } => admin::daemon_reload(socket).await,
                admin::DaemonCommand::Upgrade { timeout, socket } => {
                    admin::daemon_upgrade(socket, timeout).await
                }
                admin::DaemonCommand::Status { socket } => admin::daemon_status(socket).await,
            }
        }),
        Command::Up(args) => runtime.block_on(admin::up(args)),
        Command::Debug { command } => runtime.block_on(admin::debug(command)),
        Command::Secret { command } => runtime.block_on(admin::secret(command)),
        Command::Logging { command } => runtime.block_on(admin::logging(command)),
        Command::Deploy(args) => runtime.block_on(admin::deploy(*args)),
        Command::App { command } => runtime.block_on(async move {
            match command {
                admin::AppCommand::List { socket } => admin::app_list(socket).await,
            }
        }),
        Command::Release { command } => runtime.block_on(async move {
            match command {
                admin::ReleaseCommand::List { app, socket } => {
                    admin::release_list(app, socket).await
                }
                admin::ReleaseCommand::Wait {
                    release,
                    states,
                    timeout,
                    socket,
                } => admin::release_wait(release, states, timeout, socket).await,
            }
        }),
        Command::Host {
            command: admin::HostCommand::List { socket },
        } => runtime.block_on(admin::host_list(socket)),
        Command::Route {
            command: admin::RouteCommand::List { socket },
        } => runtime.block_on(admin::route_list(socket)),
        Command::Upstream {
            command: admin::UpstreamCommand::List { socket },
        } => runtime.block_on(admin::upstream_list(socket)),
        Command::Rollback { app, socket } => runtime.block_on(admin::rollback(app, socket)),
        Command::Drain { release, socket } => runtime.block_on(admin::drain(release, socket)),
        Command::Rollout { command } => runtime.block_on(admin::rollout(command)),
        Command::Scale(args) => runtime.block_on(admin::scale(args)),
        Command::Maintenance { command } => runtime.block_on(admin::maintenance(command)),
        Command::Dashboard { command } => runtime.block_on(admin::dashboard(command)),
        Command::Overlay { command } => runtime.block_on(admin::overlay(command)),
        Command::Status { socket } => runtime.block_on(admin::daemon_status(socket)),
        Command::Apply(args) => exit_with(runtime, admin::apply(*args)),
        Command::Config { command } => match command {
            admin::ConfigCommand::Export {
                app,
                all_releases,
                resolve_rollouts,
                json,
                socket,
            } => runtime.block_on(admin::config_export(
                app,
                all_releases,
                resolve_rollouts,
                json,
                socket,
            )),
            admin::ConfigCommand::Show {
                app,
                effective,
                socket,
            } => runtime.block_on(admin::config_show(app, effective, socket)),
            admin::ConfigCommand::Diff { file, json, socket } => {
                exit_with(runtime, admin::config_diff(file, json, socket))
            }
        },
        Command::Events {
            follow,
            after,
            socket,
        } => runtime.block_on(admin::events(follow, after, socket)),
        Command::Logs(args) => runtime.block_on(async move {
            match args.command.clone() {
                Some(admin::LogsCommand::Reopen { workers, socket }) => {
                    admin::logs_reopen(workers, socket).await
                }
                None => admin::logs(args).await,
            }
        }),
        Command::Metrics { socket } => runtime.block_on(admin::metrics(socket)),
        Command::Edge {
            command: edge::EdgeCommand::Run(args),
        } => runtime.block_on(edge::run(*args)),
    }
}

/// Run a command whose result is an exit code (section 10.4: 0 for no
/// changes, 2 for changes planned, 1 for errors) and exit with it.
fn exit_with(
    runtime: tokio::runtime::Runtime,
    command: impl std::future::Future<Output = anyhow::Result<i32>>,
) -> anyhow::Result<()> {
    let code = runtime.block_on(command)?;

    if code == 0 {
        return Ok(());
    }

    drop(runtime);
    std::process::exit(code)
}

/// Parse NAME=DURATION for --timeout.
pub(crate) fn parse_timeout(text: &str) -> Result<(String, Duration), String> {
    let (name, duration) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=DURATION, for example queue=5s"))?;
    let name = name.trim();
    if ![
        "queue",
        "deadline",
        "header_read",
        "body_idle",
        "response_idle",
        "keep_alive",
        "keep_alive_proxy",
        "soft",
        "hard",
        "hard_drain_grace",
        "shutdown",
        "loop_stall",
    ]
    .contains(&name)
    {
        return Err(format!(
            "{name:?} is not a timeout name: queue, deadline, header_read, body_idle, \
             response_idle, keep_alive, keep_alive_proxy, soft, hard, hard_drain_grace, \
             shutdown or loop_stall"
        ));
    }
    Ok((name.to_owned(), muru_config::parse_duration(duration)?))
}

/// Apply the edge's timeouts from --timeout settings to the limits.
pub(crate) fn apply_timeouts(limits: &mut muru_edge::Limits, timeouts: &[(String, Duration)]) {
    for (name, duration) in timeouts {
        match name.as_str() {
            "queue" => limits.queue_timeout = *duration,
            "deadline" => limits.request_deadline = *duration,
            "header_read" => limits.header_read_timeout = *duration,
            "body_idle" => limits.body_idle_timeout = *duration,
            "response_idle" => limits.response_idle_timeout = *duration,
            "keep_alive" => limits.keep_alive_timeout = *duration,
            "keep_alive_proxy" => limits.keep_alive_timeout_proxy = *duration,
            _ => {}
        }
    }
}

/// Parse KEY=VALUE for --memory, checking the key and the value's form.
pub(crate) fn parse_memory_setting(text: &str) -> Result<(String, String), String> {
    let (key, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not KEY=VALUE, for example soft_limit=800MB"))?;
    let (key, value) = (key.trim(), value.trim());

    match key {
        "measure" => {
            value
                .parse::<muru_config::Measure>()
                .map_err(|error| error.to_string())?;
        }
        "soft_limit" | "hard_limit" => {
            muru_config::parse_size(value)?;
        }
        "soft_for" | "check_interval" => {
            muru_config::parse_duration(value)?;
        }
        "reclaim_first" => {
            if !["true", "false"].contains(&value) {
                return Err(format!(
                    "reclaim_first must be true or false, not {value:?}"
                ));
            }
        }
        _ => {
            return Err(format!(
                "{key:?} is not a memory setting: measure, soft_limit, soft_for, reclaim_first, \
                 hard_limit or check_interval"
            ));
        }
    }

    Ok((key.to_owned(), value.to_owned()))
}

/// The memory policy from --memory settings over a project table
/// (section 7.7), with the defaults of appendix C.
pub(crate) fn memory_policy(
    settings: &[(String, String)],
    table: Option<&muru_config::Memory>,
) -> anyhow::Result<muru_controller::model::MemoryPolicy> {
    let mut table = table.cloned().unwrap_or_default();

    for (key, value) in settings {
        match key.as_str() {
            "measure" => {
                table.measure = Some(
                    value
                        .parse()
                        .map_err(|e: muru_config::ParseError| anyhow::anyhow!("{e}"))?,
                )
            }
            "soft_limit" => table.soft_limit = Some(value.clone()),
            "soft_for" => table.soft_for = Some(value.clone()),
            "reclaim_first" => table.reclaim_first = Some(value == "true"),
            "hard_limit" => table.hard_limit = Some(value.clone()),
            "check_interval" => table.check_interval = Some(value.clone()),
            _ => {}
        }
    }

    let defaults = muru_controller::model::MemoryPolicy::default();
    let size = |text: &Option<String>, name: &str| -> anyhow::Result<Option<u64>> {
        text.as_ref()
            .map(|text| {
                muru_config::parse_size(text)
                    .map_err(|error| anyhow::anyhow!("memory.{name}: {error}"))
            })
            .transpose()
    };
    let duration = |text: &Option<String>, name: &str| -> anyhow::Result<Option<u64>> {
        text.as_ref()
            .map(|text| {
                muru_config::parse_duration(text)
                    .map(|duration| duration.as_millis() as u64)
                    .map_err(|error| anyhow::anyhow!("memory.{name}: {error}"))
            })
            .transpose()
    };
    let policy = muru_controller::model::MemoryPolicy {
        measure: table.measure.unwrap_or(defaults.measure),
        soft_limit: size(&table.soft_limit, "soft_limit")?,
        soft_for_ms: duration(&table.soft_for, "soft_for")?.unwrap_or(defaults.soft_for_ms),
        reclaim_first: table.reclaim_first.unwrap_or(defaults.reclaim_first),
        hard_limit: size(&table.hard_limit, "hard_limit")?,
        check_interval_ms: duration(&table.check_interval, "check_interval")?
            .unwrap_or(defaults.check_interval_ms),
    };

    if let (Some(soft), Some(hard)) = (policy.soft_limit, policy.hard_limit) {
        anyhow::ensure!(
            hard >= soft,
            "memory.hard_limit must not be below memory.soft_limit"
        );
    }

    anyhow::ensure!(
        policy.check_interval_ms > 0,
        "memory.check_interval must be longer than nothing"
    );
    Ok(policy)
}

/// The host's timeouts (section 7.6), resolved from --timeout and a
/// project table.
pub(crate) struct HostTimeouts {
    /// How long a request may run before it is interrupted; off when absent.
    pub soft: Option<Duration>,
    /// How long a request may run before its worker is replaced; off
    /// when absent.
    pub hard: Option<Duration>,
    /// The grace for a replaced worker's other requests.
    pub hard_drain_grace: Duration,
    /// The time before a stopping worker is killed.
    pub shutdown: Duration,
    /// How long an ASGI worker's event loop may stall.
    pub loop_stall: Duration,
}

impl HostTimeouts {
    /// The timeouts as a `[timeouts]` table, for --print-config.
    pub fn table(&self) -> muru_config::Timeouts {
        muru_config::Timeouts {
            soft_timeout: self.soft.map(muru_config::format_duration),
            hard_timeout: self.hard.map(muru_config::format_duration),
            hard_drain_grace: Some(muru_config::format_duration(self.hard_drain_grace)),
            shutdown_timeout: Some(muru_config::format_duration(self.shutdown)),
            loop_stall_timeout: Some(muru_config::format_duration(self.loop_stall)),
        }
    }
}

/// The host's timeouts from --timeout and a project table (section 7.6).
pub(crate) fn worker_timeouts(
    timeouts: &[(String, Duration)],
    table: Option<&muru_config::Timeouts>,
) -> anyhow::Result<HostTimeouts> {
    let parse = |text: &Option<String>, name: &str| -> anyhow::Result<Option<Duration>> {
        match text {
            Some(text) => muru_config::parse_duration(text)
                .map(Some)
                .map_err(|error| anyhow::anyhow!("[tool.muru.timeouts] {name}: {error}")),
            None => Ok(None),
        }
    };
    let mut soft = table.map_or(Ok(None), |table| parse(&table.soft_timeout, "soft_timeout"))?;
    let mut hard = table.map_or(Ok(None), |table| parse(&table.hard_timeout, "hard_timeout"))?;
    let mut grace = table
        .map_or(Ok(None), |table| {
            parse(&table.hard_drain_grace, "hard_drain_grace")
        })?
        .unwrap_or(Duration::from_secs(10));
    let mut shutdown = table
        .map_or(Ok(None), |table| {
            parse(&table.shutdown_timeout, "shutdown_timeout")
        })?
        .unwrap_or(Duration::from_secs(5));
    let mut loop_stall = table
        .map_or(Ok(None), |table| {
            parse(&table.loop_stall_timeout, "loop_stall_timeout")
        })?
        .unwrap_or(Duration::from_secs(10));

    for (name, duration) in timeouts {
        match name.as_str() {
            "soft" => soft = Some(*duration),
            "hard" => hard = Some(*duration),
            "hard_drain_grace" => grace = *duration,
            "shutdown" => shutdown = *duration,
            "loop_stall" => loop_stall = *duration,
            _ => {}
        }
    }

    // Section 7.6: the hard timeout backs the soft one up, so it must
    // come later.

    anyhow::ensure!(
        soft != Some(Duration::ZERO),
        "soft_timeout must be longer than nothing"
    );
    anyhow::ensure!(
        loop_stall > Duration::ZERO,
        "loop_stall_timeout must be longer than nothing"
    );

    if let (Some(soft), Some(hard)) = (soft, hard) {
        anyhow::ensure!(
            hard >= soft + Duration::from_secs(5),
            "hard_timeout must exceed soft_timeout by at least 5s"
        );
    }

    Ok(HostTimeouts {
        soft,
        hard,
        hard_drain_grace: grace,
        shutdown,
        loop_stall,
    })
}

/// Largest request body in development (section 13.1, ADR 0008).
const DEVELOPMENT_MAX_BODY_SIZE: u64 = 100 * 1024 * 1024;

fn limits(args: &RunArgs) -> muru_edge::Limits {
    let defaults = muru_edge::Limits::default();
    let mut limits = muru_edge::Limits {
        max_body_size: args.max_body_size.unwrap_or(if args.allow_public {
            defaults.max_body_size
        } else {
            DEVELOPMENT_MAX_BODY_SIZE
        }),
        max_connections: args.max_connections.unwrap_or(defaults.max_connections),
        max_connections_per_client: args
            .max_connections_per_client
            .unwrap_or(defaults.max_connections_per_client),
        max_requests_per_connection: args
            .max_requests_per_connection
            .unwrap_or(defaults.max_requests_per_connection),
        ..defaults
    };
    apply_timeouts(&mut limits, &args.timeouts);
    limits
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

/// What `muructl run` runs once the command line and the project's
/// [tool.muru] table are merged: a flag wins, then the table, then the
/// defaults of section 13.1.
struct Resolved {
    entrypoint: String,
    interface: muru_config::Interface,
    factory: Option<muru_config::FactoryPhase>,
    /// Whether gRPC server reflection is served (section 6.13).
    reflection: bool,
    /// The origins whose pages may call it through gRPC-Web.
    grpc_web_origins: Vec<String>,
    processes: usize,
    threads: usize,
    sync_threads: usize,
    concurrency: usize,
    preload: muru_config::Preload,
    hooks: Option<String>,
    /// The directory the host runs in.
    project: PathBuf,
    /// The file the table came from, when there was one.
    source: Option<PathBuf>,
    /// Named worker groups from the table (section 7.1).
    groups: std::collections::BTreeMap<String, muru_config::Group>,
    /// Named mounts from the table (section 6.7).
    mounts: std::collections::BTreeMap<String, muru_config::Mount>,
    /// Logging defaults from the table (section 11.4).
    logging: Option<muru_config::Logging>,
    /// Requests a worker serves before it is replaced (section 7.7).
    max_requests: Option<u64>,
    /// How long a worker lives before it is replaced (section 7.7).
    max_lifetime: Option<Duration>,
    /// The host's timeouts from the table (section 7.6).
    timeouts: Option<muru_config::Timeouts>,
    /// Section 4.10: the default mount's redirect settings from the table.
    accel_redirect: bool,
    cgi_local_redirect: bool,
    /// Section 4.9: the default mount's file hints by path from the table.
    sendfile_roots: Vec<String>,
    sendfile_header: Option<String>,
    /// Section 4.12: the error pages, override and maintenance
    /// exemptions from the table.
    error_pages: std::collections::BTreeMap<String, muru_config::ErrorPage>,
    error_override: Option<muru_config::ErrorOverride>,
    maintenance: Option<muru_config::Maintenance>,
    /// The memory table (section 7.7).
    memory: Option<muru_config::Memory>,
    /// Environment variables from the table (section 10.8).
    env: std::collections::BTreeMap<String, muru_config::EnvValue>,
    /// The scaling table (sections 7.2 and 7.4), written out by
    /// --print-config; a development run never scales.
    scaling: Option<muru_config::Scaling>,
}

/// Defaults for development runs (section 13.1): one worker with a few threads.
const DEFAULT_PROCESSES: usize = 1;
const DEFAULT_THREADS: usize = 4;
/// Defaults for ASGI workers (section 6.6): the sync pool and the loop's
/// concurrency, the latter the mount default of appendix C.
const DEFAULT_SYNC_THREADS: usize = 8;
const DEFAULT_CONCURRENCY: usize = 1000;

fn resolve(args: &RunArgs) -> anyhow::Result<Resolved> {
    let chdir = args.chdir.clone();

    let (entrypoint, project) = match args.app.as_deref() {
        Some(app) if Path::new(app).is_dir() => {
            anyhow::ensure!(
                chdir.is_none(),
                "give the project as the argument or with --chdir, not both"
            );
            (None, PathBuf::from(app))
        }
        Some(app)
            if Path::new(app)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| muru_config::PROJECT_FILES.contains(&name)) =>
        {
            let path = Path::new(app);
            anyhow::ensure!(path.is_file(), "{app} does not exist");
            anyhow::ensure!(
                chdir.is_none(),
                "give the project as the argument or with --chdir, not both"
            );
            (
                None,
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
            )
        }
        Some(app) => {
            anyhow::ensure!(
                !app.contains('/') && !app.ends_with(".py"),
                "{app} is neither module:callable nor a project directory"
            );
            (
                Some(app.to_owned()),
                chdir.unwrap_or_else(|| PathBuf::from(".")),
            )
        }
        None => (None, chdir.unwrap_or_else(|| PathBuf::from("."))),
    };

    let (source, table) = match muru_config::load_project(&project)? {
        Some((path, table)) => (Some(path), table),
        None => (None, muru_config::ToolMuru::default()),
    };

    // Whether the entrypoint is a factory belongs to the entrypoint: the
    // table's factory settings apply to the table's entrypoint only.

    let from_table = entrypoint.is_none();

    let entrypoint = match entrypoint.or(table.entrypoint) {
        Some(entrypoint) => entrypoint,
        None => match &source {
            Some(path) => anyhow::bail!(
                "{} has no entrypoint in its [tool.muru] table; give one as module:callable",
                path.display()
            ),
            None => anyhow::bail!(
                "no entrypoint: give module:callable, or a project directory with a [tool.muru] table in its pyproject.toml or muru.toml"
            ),
        },
    };

    let factory = if args.factory || (from_table && table.factory == Some(true)) {
        Some(
            table
                .factory_phase
                .filter(|_| from_table)
                .unwrap_or(muru_config::FactoryPhase::Worker),
        )
    } else {
        None
    };
    muru_config::check_origins(&table.grpc_web_origins).map_err(|error| anyhow::anyhow!(error))?;

    for (name, mount) in &table.mounts {
        muru_config::check_mount_threads(mount.threads, mount.interface)
            .map_err(|error| anyhow::anyhow!("mount {name}: {error}"))?;
    }

    Ok(Resolved {
        entrypoint,
        interface: args
            .interface
            .or(table.interface)
            .unwrap_or(muru_config::Interface::Auto),
        factory,
        reflection: table.reflection.unwrap_or(false),
        grpc_web_origins: table.grpc_web_origins.clone(),
        processes: args
            .processes
            .or(table.processes)
            .unwrap_or(DEFAULT_PROCESSES),
        threads: args.threads.or(table.threads).unwrap_or(DEFAULT_THREADS),
        sync_threads: args
            .sync_threads
            .or(table.sync_threads)
            .unwrap_or(DEFAULT_SYNC_THREADS),
        concurrency: args
            .concurrency
            .or(table.concurrency)
            .unwrap_or(DEFAULT_CONCURRENCY),
        preload: args
            .preload
            .or(table.preload)
            .unwrap_or(muru_config::Preload::Auto),
        hooks: args.hooks.clone().or(table.hooks),
        project,
        source,
        groups: table.groups,
        mounts: table.mounts,
        accel_redirect: table.accel_redirect.unwrap_or(false),
        cgi_local_redirect: table.cgi_local_redirect.unwrap_or(false),
        sendfile_roots: table.sendfile_roots,
        sendfile_header: table.sendfile_header,
        error_pages: table.error_pages,
        error_override: table.error_override,
        maintenance: table.maintenance,
        logging: table.logging,
        max_requests: args.max_requests.or(table.max_requests),
        max_lifetime: match (args.max_lifetime, &table.max_lifetime) {
            (Some(lifetime), _) => Some(lifetime),
            (None, Some(text)) => Some(
                muru_config::parse_duration(text)
                    .map_err(|error| anyhow::anyhow!("[tool.muru] max_lifetime: {error}"))?,
            ),
            (None, None) => None,
        },
        timeouts: table.timeouts,
        memory: table.memory,
        env: table.env,
        scaling: table.scaling,
    })
}

/// The declarative file equivalent to this run, for --print-config: the
/// release as section 6.8 would define it, the listeners and the limits.
fn declarative(args: &RunArgs, resolved: &Resolved) -> anyhow::Result<String> {
    use muru_config::{App, Document, Listener, Release};

    let name = app_name(&resolved.entrypoint).replace('_', "-");
    let limits = limits(args);
    let (http, https) = addresses(args);
    let mut listeners = vec![Listener {
        address: http.to_string(),
        tls: false,
        h2c: None,
    }];

    if args.https || args.https_only || args.tls_cert.is_some() {
        listeners.push(Listener {
            address: https.to_string(),
            tls: true,
            h2c: None,
        });
    }

    let release = Release {
        grpc_web_origins: resolved.grpc_web_origins.clone(),
        kind: None,
        profile: None,
        command: Vec::new(),
        cwd: None,
        bind: None,
        instances: None,
        readiness: None,
        stop_signal: None,
        source: Some(resolved.project.display().to_string()),
        entrypoint: resolved.entrypoint.clone(),
        interface: (resolved.interface != muru_config::Interface::Auto)
            .then_some(resolved.interface),
        reflection: resolved.reflection.then_some(true),
        factory: resolved.factory.map(|_| true),
        factory_phase: resolved.factory,
        python: Some(
            python_interpreter(args.python.clone(), &resolved.project)
                .display()
                .to_string(),
        ),
        processes: Some(resolved.processes),
        threads: Some(resolved.threads),
        sync_threads: Some(resolved.sync_threads),
        concurrency: Some(resolved.concurrency),
        preload: Some(resolved.preload),
        hooks: resolved.hooks.clone(),
        groups: resolved.groups.clone(),
        mounts: resolved.mounts.clone(),
        accel_redirect: resolved.accel_redirect.then_some(true),
        cgi_local_redirect: resolved.cgi_local_redirect.then_some(true),
        sendfile_roots: resolved.sendfile_roots.clone(),
        sendfile_header: resolved.sendfile_header.clone(),
        error_pages: resolved.error_pages.clone(),
        error_override: resolved.error_override.clone(),
        maintenance: resolved.maintenance.clone(),
        logging: resolved.logging.clone(),
        max_requests: resolved.max_requests,
        max_lifetime: resolved.max_lifetime.map(muru_config::format_duration),
        env: resolved.env.clone(),
        scaling: resolved.scaling.clone(),
        timeouts: {
            let timeouts = worker_timeouts(&args.timeouts, resolved.timeouts.as_ref())?;
            (timeouts.soft.is_some() || timeouts.hard.is_some() || resolved.timeouts.is_some())
                .then(|| timeouts.table())
        },
        memory: {
            let policy = memory_policy(&args.memory, resolved.memory.as_ref())?;
            (policy != muru_controller::model::MemoryPolicy::default()).then(|| {
                muru_config::Memory {
                    measure: Some(policy.measure),
                    soft_limit: policy.soft_limit.map(muru_config::format_size),
                    soft_for: Some(muru_config::format_duration(Duration::from_millis(
                        policy.soft_for_ms,
                    ))),
                    reclaim_first: Some(policy.reclaim_first),
                    hard_limit: policy.hard_limit.map(muru_config::format_size),
                    check_interval: Some(muru_config::format_duration(Duration::from_millis(
                        policy.check_interval_ms,
                    ))),
                }
            })
        },
    };
    let document = Document {
        listener: listeners,
        compression: args.no_compression.then(|| muru_config::Compression {
            enabled: Some(false),
            ..Default::default()
        }),
        limits: Some(muru_config::Limits {
            max_body_size: Some(muru_config::format_size(limits.max_body_size)),
            max_header_bytes: Some(muru_config::format_size(limits.max_header_bytes as u64)),
            max_headers: Some(limits.max_headers),
            header_read_timeout: Some(muru_config::format_duration(limits.header_read_timeout)),
            queue_timeout: Some(muru_config::format_duration(limits.queue_timeout)),
            request_deadline: Some(muru_config::format_duration(limits.request_deadline)),
            body_idle_timeout: Some(muru_config::format_duration(limits.body_idle_timeout)),
            response_idle_timeout: Some(muru_config::format_duration(limits.response_idle_timeout)),
            max_connections: Some(limits.max_connections),
            max_connections_per_client: Some(limits.max_connections_per_client),
            keep_alive_timeout: Some(muru_config::format_duration(limits.keep_alive_timeout)),
            keep_alive_timeout_proxy: Some(muru_config::format_duration(
                limits.keep_alive_timeout_proxy,
            )),
            max_requests_per_connection: Some(limits.max_requests_per_connection),
            http2: Some(muru_config::Http2Limits {
                max_concurrent_streams: Some(limits.http2_max_concurrent_streams),
            }),
            spool_quota: None,
        }),
        app: std::collections::BTreeMap::from([(
            name,
            App {
                release: std::collections::BTreeMap::from([("r1".to_owned(), release)]),
                ..App::default()
            },
        )]),
        ..Document::default()
    };
    let text = document.to_toml().context("writing the configuration")?;
    Ok(format!(
        "# The declarative equivalent of this muructl run, for muructl apply.\n\
         # The release is written as r1; muructl deploy numbers releases itself.\n\
         # Listeners and limits belong in the daemon configuration file.\n\n{text}"
    ))
}

/// A host's named groups as the edge connects them: each group's sockets
/// with the paths that select it.
fn group_sockets(
    host: &HostProcess,
    groups: &std::collections::BTreeMap<String, muru_config::Group>,
) -> Vec<muru_edge::routing::GroupSockets> {
    group_sockets_from(&host.group_sockets, groups)
}

/// Named groups' sockets, as the host announced them, with the paths
/// each group serves from the project table.
fn group_sockets_from(
    announced: &[(String, Vec<PathBuf>)],
    groups: &std::collections::BTreeMap<String, muru_config::Group>,
) -> Vec<muru_edge::routing::GroupSockets> {
    announced
        .iter()
        .map(|(name, sockets)| muru_edge::routing::GroupSockets {
            name: name.clone(),
            paths: groups
                .get(name)
                .map(|group| group.paths.clone())
                .unwrap_or_default(),
            client_classes: groups
                .get(name)
                .and_then(|group| group.matches.as_ref())
                .map(|matches| matches.client_class.clone())
                .unwrap_or_default(),
            sockets: sockets.clone(),
        })
        .collect()
}

/// Connect to a host's control socket for the run (section 5.9), or,
/// when the host has none, a receiver that never yields.
async fn host_control(
    host: &HostProcess,
) -> (
    Option<muru_controller::host::HostControl>,
    tokio::sync::mpsc::Receiver<muru_controller::host::HostMessage>,
) {
    match muru_controller::host::HostControl::connect(&host.control_path()).await {
        Ok((control, messages)) => (Some(control), messages),
        Err(error) => {
            println!("muru: no control connection to the host: {error:#}");
            let (_sender, receiver) = tokio::sync::mpsc::channel(1);
            std::mem::forget(_sender);
            (None, receiver)
        }
    }
}

/// The named mounts as the edge selects them, with their redirect
/// settings (section 4.10), the release's where a mount is silent.
fn mount_paths(resolved: &Resolved) -> Vec<muru_edge::routing::MountPath> {
    resolved
        .mounts
        .iter()
        .map(|(name, mount)| muru_edge::routing::MountPath {
            name: name.clone(),
            path: mount.path.clone(),
            internal: mount.internal.unwrap_or(false),
            accept_redirects_from: mount.accept_redirects_from.clone(),
            accel_redirect: mount.accel_redirect.unwrap_or(resolved.accel_redirect),
            cgi_local_redirect: mount
                .cgi_local_redirect
                .unwrap_or(resolved.cgi_local_redirect),
            sendfile: muru_edge::routing::Sendfile::new(
                mount
                    .sendfile_roots
                    .as_deref()
                    .unwrap_or(&resolved.sendfile_roots),
                mount
                    .sendfile_header
                    .as_deref()
                    .or(resolved.sendfile_header.as_deref())
                    .unwrap_or_default(),
            ),
        })
        .collect()
}

/// The application's name: the top-level module of its entrypoint, as
/// the host names it for muru.context().
fn app_name(entrypoint: &str) -> String {
    let module = entrypoint.split(':').next().unwrap_or(entrypoint);
    let top = module.split('.').next().unwrap_or(module);

    if top.is_empty() {
        "default".to_owned()
    } else {
        top.to_owned()
    }
}

/// How TLS was set up: the listener configuration and, for a development
/// certificate, where it came from.
struct Tls {
    https: muru_edge::Https,
    development: Option<devcert::DevCertificate>,
}

/// The HTTPS listener for --https, --https-only or --tls-cert (section 13.1).
fn tls(args: &RunArgs) -> anyhow::Result<Option<Tls>> {
    if !(args.https || args.https_only || args.tls_cert.is_some()) {
        return Ok(None);
    }

    let (chain, key, development) = match (&args.tls_cert, &args.tls_key) {
        (Some(certificate), Some(key)) => {
            let (chain, key) = muru_edge::tls::load_pem_files(certificate, key)?;
            (chain, key, None)
        }
        _ => {
            let certificate = devcert::obtain(&devcert::cache_dir()?, &args.hosts)?;
            let (chain, key) =
                muru_edge::tls::load_pem_files(&certificate.certificate, &certificate.key)?;
            (chain, key, Some(certificate))
        }
    };

    Ok(Some(Tls {
        https: muru_edge::Https {
            listen: addresses(args).1,
            config: muru_edge::tls::server_config(chain, key)?,
            only: args.https_only,
        },
        development,
    }))
}

/// The interpreter to run the host with (section 13.1): --python, else the
/// active virtualenv, else the project's own .venv, else python3 on PATH. A
/// path is made absolute, because a virtualenv's interpreter locates its
/// site-packages from its own path.
fn python_interpreter(explicit: Option<PathBuf>, project: &Path) -> PathBuf {
    let python = explicit
        .or_else(|| {
            std::env::var_os("VIRTUAL_ENV")
                .map(|venv| PathBuf::from(venv).join("bin").join("python"))
        })
        .or_else(|| {
            let venv = project.join(".venv").join("bin").join("python");
            venv.is_file().then_some(venv)
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
    let mut resolved = resolve(&args)?;
    let development = !args.allow_public;

    if !args.bind.ip.is_loopback() && development {
        anyhow::bail!(
            "muructl run serves loopback addresses only, because it is development mode: error \
             pages show tracebacks and nothing is persisted. --allow-public serves {} with those \
             behaviours off, but production is the daemon's job (section 13.3, phase 2)",
            args.bind.ip
        );
    }

    anyhow::ensure!(resolved.processes >= 1, "--processes must be at least 1");
    anyhow::ensure!(resolved.threads >= 1, "--threads must be at least 1");
    anyhow::ensure!(
        resolved.sync_threads >= 1,
        "--sync-threads must be at least 1"
    );
    anyhow::ensure!(
        resolved.concurrency >= 1,
        "--concurrency must be at least 1"
    );

    // The stop signals are caught before the host starts, so that a Ctrl-C
    // or SIGTERM arriving while the workers are coming up stops the run in
    // order instead of killing muructl and leaving the host to notice.

    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("installing the SIGINT handler")?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;

    if args.single {
        resolved.processes = 1;
        resolved.threads = 1;
    }

    if args.print_config {
        print!("{}", declarative(&args, &resolved)?);
        return Ok(());
    }

    if let Some(source) = &resolved.source {
        println!("muru: using [tool.muru] from {}", source.display());
    }

    // Certificates are sorted out before anything is started, so a bad
    // --tls-cert or an unwritable cache fails with nothing to clean up.

    let tls = tls(&args)?;
    let run_dir = std::env::temp_dir().join(format!("muru-{}", std::process::id()));
    std::fs::create_dir_all(&run_dir).with_context(|| format!("creating {}", run_dir.display()))?;
    let access_log = access_log_config(&args);
    let timeouts = worker_timeouts(&args.timeouts, resolved.timeouts.as_ref())?;
    let spec = HostSpec {
        python: python_interpreter(args.python.clone(), &resolved.project),
        app: resolved.entrypoint.clone(),
        name: None,
        release: None,
        interface: resolved.interface,
        factory: resolved.factory,
        reflection: resolved.reflection,
        processes: resolved.processes,
        threads: resolved.threads,
        sync_threads: resolved.sync_threads,
        concurrency: resolved.concurrency,
        preload: resolved.preload,
        hooks: resolved.hooks.clone(),
        chdir: Some(resolved.project.clone()),
        tcp_base_port: (args.link == Link::Tcp).then_some(args.tcp_base_port),
        single: args.single,
        detach_to: None,
        groups: resolved
            .groups
            .iter()
            .map(|(name, group)| muru_controller::host::HostGroup {
                name: name.clone(),
                processes: group.processes.unwrap_or(resolved.processes),
                threads: group.threads.unwrap_or(resolved.threads),
                sync_threads: group.sync_threads.unwrap_or(resolved.sync_threads),
                concurrency: group.concurrency.unwrap_or(resolved.concurrency),
            })
            .collect(),
        mounts: resolved
            .mounts
            .iter()
            .map(|(name, mount)| muru_controller::host::HostMount {
                name: name.clone(),
                path: mount.path.clone(),
                entrypoint: mount.entrypoint.clone(),
                interface: mount.interface.unwrap_or(muru_config::Interface::Auto),
                threads: mount.threads,
            })
            .collect(),
        logging: resolved
            .logging
            .as_ref()
            .and_then(|logging| logging.python.clone())
            .unwrap_or_default(),
        max_requests: resolved.max_requests,
        max_lifetime: resolved.max_lifetime,
        soft_timeout: timeouts.soft,
        hard_timeout: timeouts.hard,
        hard_drain_grace: timeouts.hard_drain_grace,
        shutdown_timeout: timeouts.shutdown,
        loop_stall_timeout: timeouts.loop_stall,
        memory: memory_policy(&args.memory, resolved.memory.as_ref())?,
        env: resolved
            .env
            .iter()
            .map(|(name, value)| match value {
                muru_config::EnvValue::Plain(text) => Ok((name.clone(), text.clone())),
                muru_config::EnvValue::Secret(_) => Err(anyhow::anyhow!(
                    "[tool.muru.env] {name}: secrets are resolved by the daemon; a development \
                     run cannot deliver them"
                )),
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        identity: None,
    };

    let logs = Logs::default();
    let group_tables = resolved.groups.clone();
    let mounts = mount_paths(&resolved);
    let redirect_flags = muru_edge::routing::RedirectFlags {
        accel_redirect: resolved.accel_redirect,
        cgi_local_redirect: resolved.cgi_local_redirect,
    };
    let sendfile = muru_edge::routing::Sendfile::new(
        &resolved.sendfile_roots,
        resolved.sendfile_header.as_deref().unwrap_or_default(),
    );

    // Section 4.12: the project's error pages, with files under it.

    muru_config::check_error_pages(&resolved.error_pages, true)
        .map_err(|error| anyhow::anyhow!("[tool.muru] {error}"))?;

    if let Some(table) = &resolved.error_override {
        muru_config::check_error_override(table)
            .map_err(|error| anyhow::anyhow!("[tool.muru] {error}"))?;
    }

    let error_policy = Arc::new(
        muru_edge::errors::ErrorPolicy::load(
            &muru_controller::daemon::error_pages_message(
                &resolved.error_pages,
                Some(&resolved.project),
            ),
            resolved
                .error_override
                .as_ref()
                .map(muru_controller::daemon::error_override_message)
                .as_ref(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("[tool.muru] {error}"))?,
    );
    let mut current = HostProcess::start(&spec, &run_dir, 1, &logs)?;

    if let Err(error) = current.wait_ready(START_TIMEOUT).await {
        current.stop().await;
        let _ = std::fs::remove_dir_all(&run_dir);
        return Err(error);
    }

    let (mut control, mut host_messages) = host_control(&current).await;

    let (http_address, https_address) = addresses(&args);

    let label = app_name(&resolved.entrypoint).replace('_', "-");

    let edge = match muru_edge::Edge::bind(muru_edge::Config {
        listen: http_address,
        app: label.clone(),
        release: "r1".to_owned(),
        sockets: current.default_sockets(),
        groups: group_sockets(&current, &resolved.groups),
        mounts: mounts.clone(),
        accel_redirect: resolved.accel_redirect,
        cgi_local_redirect: resolved.cgi_local_redirect,
        sendfile: Arc::clone(&sendfile),
        grpc_web_origins: resolved.grpc_web_origins.clone(),
        errors: Arc::clone(&error_policy),
        development,
        access_log,
        limits: limits(&args),
        compression: if args.no_compression {
            muru_edge::compression::Compression::off()
        } else {
            muru_edge::compression::Compression::default()
        },
        https: tls.as_ref().map(|tls| tls.https.clone()),
        spool: None,
    })
    .await
    {
        Ok(edge) => edge,
        Err(error) => {
            current.stop().await;
            let _ = std::fs::remove_dir_all(&run_dir);
            return Err(explain_bind_error(error, https_address));
        }
    };

    let addresses = match edge.https_addr() {
        Some(https) if args.https_only => {
            format!(
                "https://{https} (http://{} redirects to it)",
                edge.local_addr()
            )
        }
        Some(https) => format!("http://{} and https://{https}", edge.local_addr()),
        None => format!("http://{}", edge.local_addr()),
    };

    // The workers said in HELLO what they are and how much they take at
    // once, which is also what the edge dispatches by.

    let capacity = match edge.pools().current().default_pool().workers().first() {
        Some(worker) if worker.worker_type == "asgi" => format!("concurrency {}", worker.slots),
        Some(worker) => format!("{} threads", worker.slots),
        None => format!("{} threads", resolved.threads),
    };

    println!(
        "muru: serving {} on {addresses} with {} workers of {capacity}",
        resolved.entrypoint, resolved.processes
    );

    if development {
        println!(
            "muru: development mode: error pages show tracebacks, nothing is persisted and only \
             loopback is served; production runs under the daemon, which arrives in phase 2"
        );
    } else {
        println!(
            "muru: --allow-public: development behaviours are off, but muructl run is not the \
             supported way to run in production; that is the daemon, which arrives in phase 2"
        );
    }

    if let Some(tls) = &tls {
        match (&tls.development, &args.tls_cert) {
            (Some(certificate), _) => println!("{}", certificate.describe()),
            (None, Some(path)) => println!("muru: TLS certificate from {}", path.display()),
            (None, None) => {}
        }
    }

    let release_number = Arc::new(AtomicU32::new(1));

    let dashboard = match args.dashboard.as_deref() {
        Some("off") => None,
        Some(address) => Some(address.to_owned()),
        None if development => Some("127.0.0.1:8081".to_owned()),
        None => None,
    };

    if let Some(address) = dashboard {
        match dashboard::start(
            &address,
            dashboard::Inputs {
                development,
                app: resolved.entrypoint.clone(),
                listen: edge.local_addr(),
                https: edge.https_addr(),
                started: std::time::Instant::now(),
                release: Arc::clone(&release_number),
                pools: edge.pools(),
                connections: edge.connections(),
                access_log: edge.access_log(),
                logs: logs.clone(),
            },
        )
        .await
        {
            Ok(address) => println!("muru: dashboard on http://{address}/"),
            Err(error) => println!("muru: no dashboard: {error:#}"),
        }
    }

    let (changed_tx, mut changed) = mpsc::channel::<PathBuf>(64);

    if args.reload {
        let directories = if args.reload_dir.is_empty() {
            vec![resolved.project.clone()]
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

    // Ctrl-C and SIGTERM both stop the run in order: the host is told to
    // stop and its workers finish what they are serving.

    let outcome = loop {
        tokio::select! {
            served = &mut serving => {
                break served.map_err(anyhow::Error::from).and_then(|result| result);
            }
            _ = interrupt.recv() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
            status = current.exited() => {
                break Err(anyhow::anyhow!("the host exited with {}", status?));
            }
            Some(message) = host_messages.recv() => {
                // Sections 7.6 and 7.7: the host replaced a worker, so
                // the edge routes to the new socket, and the host hears
                // that it does before it stops the old one.

                match message {
                    muru_controller::host::HostMessage::Workers { version, sockets, groups } => {
                        let connected = muru_edge::routing::AppPools::connect(
                            &label,
                            &format!("r{}", current.release),
                            &sockets,
                            &group_sockets_from(&groups, &group_tables),
                            &mounts,
                            redirect_flags,
                        )
                        .await
                        .map(|pools| {
                            pools
                                .with_errors(Arc::clone(&error_policy))
                                .with_sendfile(Arc::clone(&sendfile))
                                .with_grpc_web(&resolved.grpc_web_origins)
                        });

                        match connected {
                            Ok(connected) => {
                                pools.replace(connected);

                                if let Some(control) = &control {
                                    control.acknowledge(version);
                                }
                            }
                            Err(error) => {
                                println!("muru: the workers could not be reconnected: {error:#}");
                            }
                        }
                    }
                    muru_controller::host::HostMessage::Event { kind, message, .. } => {
                        println!("muru: {kind}: {message}");
                    }
                    muru_controller::host::HostMessage::Status { .. }
                    | muru_controller::host::HostMessage::Response { .. } => {}
                }
            }
            Some(path) = changed.recv() => {
                let mut paths = vec![path];
                settle(&mut changed, &mut paths).await;
                println!("muru: reloading, {} changed", describe(&paths));

                loop {
                    release += 1;

                    let mut next = match HostProcess::start(&spec, &run_dir, release, &logs) {
                        Ok(next) => next,
                        Err(error) => {
                            println!("muru: reload failed: {error:#}; release r{} keeps serving", current.release);
                            break;
                        }
                    };

                    tokio::select! {
                        ready = next.wait_ready(START_TIMEOUT) => match ready {
                            Ok(()) => {
                                let connected = muru_edge::routing::AppPools::connect(
                                    &label,
                                    &format!("r{release}"),
                                    &next.default_sockets(),
                                    &group_sockets(&next, &group_tables),
                                    &mounts,
                                    redirect_flags,
                                )
                                .await
                                .map(|pools| {
                                    pools
                                        .with_errors(Arc::clone(&error_policy))
                                        .with_sendfile(Arc::clone(&sendfile))
                                        .with_grpc_web(&resolved.grpc_web_origins)
                                });

                                match connected {
                                    Ok(connected) => pools.replace(connected),
                                    Err(error) => {
                                        println!("muru: reload failed: {error:#}; release r{} keeps serving", current.release);
                                        next.stop().await;
                                        break;
                                    }
                                }

                                println!("muru: switched to release r{release}");
                                release_number.store(release, Ordering::Relaxed);
                                let previous = std::mem::replace(&mut current, next);
                                tokio::spawn(previous.stop());
                                (control, host_messages) = host_control(&current).await;
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
