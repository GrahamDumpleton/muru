//! The commands that run or talk to the daemon (design specification
//! sections 10.4, 13.2 and 13.3, appendix A): `daemon run`, `daemon
//! start`, `daemon stop`, `daemon status`, `up`, `deploy`, `app list`,
//! `release list`, `release wait`, `rollback`, `drain`, `status`,
//! `events`, `logs`, `logs reopen`, `apply`, `config export` and `config
//! diff`.

use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use clap::{Args, Subcommand};
use muru_controller::client;
use muru_controller::locations::{Locations, Options};
use muru_proto::admin::v1 as pb;
use tokio_stream::StreamExt;

/// Where the daemon keeps things, as section 13.8 lets a command line say.
#[derive(Args, Clone, Default)]
pub struct LocationArgs {
    /// The daemon configuration file.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// The state directory: the state store and durable files.
    #[arg(long, global = true, value_name = "DIR")]
    state_dir: Option<PathBuf>,

    /// The runtime directory: the admin socket and the release run directories.
    #[arg(long, global = true, value_name = "DIR")]
    run_dir: Option<PathBuf>,

    /// The log directory.
    #[arg(long, global = true, value_name = "DIR")]
    log_dir: Option<PathBuf>,

    /// The cache directory.
    #[arg(long, global = true, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Use the system locations (/etc/muru, /var/lib/muru and so on)
    /// whoever runs the command.
    #[arg(long, global = true)]
    system: bool,
}

impl LocationArgs {
    /// Resolve the locations for these options and the environment.
    pub fn resolve(&self) -> Locations {
        Locations::resolve(&Options {
            config: self.config.clone(),
            state_dir: self.state_dir.clone(),
            run_dir: self.run_dir.clone(),
            log_dir: self.log_dir.clone(),
            cache_dir: self.cache_dir.clone(),
            system: self.system,
        })
    }

    /// The same options, for a daemon started in the background.
    fn as_arguments(&self) -> Vec<String> {
        let mut arguments = Vec::new();
        for (flag, value) in [
            ("--config", &self.config),
            ("--state-dir", &self.state_dir),
            ("--run-dir", &self.run_dir),
            ("--log-dir", &self.log_dir),
            ("--cache-dir", &self.cache_dir),
        ] {
            if let Some(path) = value {
                arguments.push(flag.to_owned());
                arguments.push(path.display().to_string());
            }
        }

        if self.system {
            arguments.push("--system".to_owned());
        }

        arguments
    }
}

/// How to reach a daemon: its admin socket.
#[derive(Args, Clone, Default)]
pub struct SocketArgs {
    /// The daemon's admin socket (default: MURU_SOCKET, else the socket of
    /// the user daemon in the runtime directory).
    #[arg(long, global = true, value_name = "PATH", env = "MURU_SOCKET")]
    socket: Option<PathBuf>,

    #[command(flatten)]
    locations: LocationArgs,
}

impl SocketArgs {
    /// The socket to connect to.
    pub fn socket(&self) -> PathBuf {
        self.socket
            .clone()
            .unwrap_or_else(|| self.locations.resolve().admin_socket())
    }
}

/// The daemon subcommands.
#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground.
    Run(DaemonRunArgs),

    /// Start the daemon in the background and wait until it answers.
    Start(DaemonRunArgs),

    /// Stop the daemon, leaving hosts and the edge serving for the next
    /// daemon to re-adopt; with --shutdown, drain and stop everything.
    Stop {
        /// Drain every release, stop the edge and the hosts, then exit.
        #[arg(long)]
        shutdown: bool,

        /// How long to wait for the daemon to exit.
        #[arg(long, value_name = "DURATION", default_value = "90s", value_parser = muru_config::parse_duration)]
        timeout: Duration,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Re-read the daemon's configuration file for what can change while
    /// it runs: the [proxy], [compression], [error_pages],
    /// [error_override], [[client_class]] and [rate_limit] tables.
    Reload {
        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Re-execute the daemon from the installed muructl and replace the
    /// edge, without interrupting traffic: hosts and the edge keep
    /// serving and the new daemon takes them over.
    Upgrade {
        /// How long to wait for the upgraded daemon to answer with a
        /// new edge.
        #[arg(long, value_name = "DURATION", default_value = "120s", value_parser = muru_config::parse_duration)]
        timeout: Duration,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Show the daemon's status.
    Status {
        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// What `daemon run` and `daemon start` take.
#[derive(Args, Clone)]
pub struct DaemonRunArgs {
    /// Serve plain HTTP on this address alone, instead of the [[listener]]
    /// entries of the configuration file (default 127.0.0.1:8080).
    #[arg(long, value_name = "ADDRESS")]
    listen: Option<SocketAddr>,

    /// Serve Prometheus metrics at /metrics on this address, instead of
    /// the [metrics] table's listen address (none by default).
    #[arg(long, value_name = "ADDRESS")]
    metrics: Option<SocketAddr>,

    /// Serve the read-only dashboard and JSON gateway on this loopback
    /// address, instead of the [dashboard] table's listen address (none
    /// by default).
    #[arg(long, value_name = "ADDRESS")]
    dashboard: Option<SocketAddr>,

    /// The service user a daemon running as root starts the edge, and
    /// the applications that name no user, as; instead of the [daemon]
    /// table's user (default muru). The account must exist and must not
    /// be root.
    #[arg(long, value_name = "NAME")]
    user: Option<String>,

    /// Set by a daemon that re-executed itself for an upgrade: the
    /// version it ran before.
    #[arg(long, hide = true, value_name = "VERSION")]
    upgraded_from: Option<String>,

    #[command(flatten)]
    locations: LocationArgs,
}

/// The arguments a daemon re-executes itself with on an upgrade: its own,
/// with --upgraded-from naming this version in place of any earlier one.
fn upgrade_arguments() -> Vec<std::ffi::OsString> {
    let mut arguments: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();

    if let Some(at) = arguments
        .iter()
        .position(|argument| argument == "--upgraded-from")
    {
        let end = (at + 2).min(arguments.len());
        arguments.drain(at..end);
    }

    arguments.push("--upgraded-from".into());
    arguments.push(env!("CARGO_PKG_VERSION").into());
    arguments
}

/// Run the daemon in the foreground.
pub async fn daemon_run(args: DaemonRunArgs) -> anyhow::Result<()> {
    let locations = args.locations.resolve();
    let document = read_config(&locations.config.path)?;

    let listeners = match args.listen {
        Some(listen) => vec![(listen, false)],
        None => listeners_from(&document)?,
    };

    let limits = limits_from(&document)?;
    let metrics = metrics_from(&document, args.metrics)?;
    let dashboard = dashboard_from(&document, args.dashboard)?;
    let sinks = sinks_from(&document)?;
    let overlay_webhook = document
        .overlay_webhook
        .as_ref()
        .map(muru_controller::triggers::Webhook::from_config)
        .transpose()
        .map_err(|error| anyhow::anyhow!("[overlay_webhook]: {error}"))?;

    // The edge writes the access log to a file in the log directory as
    // JSON lines.

    std::fs::create_dir_all(&locations.logs.path)
        .with_context(|| format!("creating {}", locations.logs.path.display()))?;
    let access_log = Some(locations.logs.path.join("access.log"));

    for (name, location) in [
        ("config", &locations.config),
        ("state", &locations.state),
        ("run", &locations.run),
        ("logs", &locations.logs),
        ("cache", &locations.cache),
    ] {
        println!(
            "muru: {name}: {} ({})",
            location.path.display(),
            location.rule
        );
    }
    let secrets = muru_controller::secrets::Store::from_document(
        &document,
        &locations.state.path.join("secrets"),
    )?;
    let service_user = args.user.clone().or_else(|| {
        document
            .daemon
            .as_ref()
            .and_then(|table| table.user.clone())
    });
    muru_controller::daemon::run(muru_controller::daemon::Config {
        locations,
        listeners,
        limits,
        service_user,
        debug_exec: document
            .daemon
            .as_ref()
            .and_then(|table| table.debug_exec)
            .unwrap_or(false),
        budget: muru_controller::budget::Budget::from_table(
            document
                .daemon
                .as_ref()
                .and_then(|table| table.budget.as_ref()),
        )
        .map_err(|error| anyhow::anyhow!("[daemon.budget]: {error}"))?,
        proxy: document.proxy.clone(),
        compression: document.compression.clone(),
        error_pages: document.error_pages.clone(),
        error_override: document.error_override.clone(),
        client_classes: document.client_class.clone(),
        rate_limits: document.rate_limit.clone(),
        secrets,
        rotation: muru_controller::rotation::Policy::from_table(document.logs.as_ref())?,
        access_log,
        metrics,
        dashboard,
        dashboard_actions: document
            .dashboard
            .as_ref()
            .is_some_and(|dashboard| dashboard.actions),
        overlay_webhook,
        sinks,
        executable: std::env::current_exe().context("finding the muructl executable")?,
        upgrade_arguments: upgrade_arguments(),
        upgraded_from: args.upgraded_from.clone(),
    })
    .await
}

/// Start the daemon in the background: a detached `muructl daemon run`
/// writing to the daemon log, waited for until its socket answers.
pub async fn daemon_start(args: DaemonRunArgs) -> anyhow::Result<()> {
    let locations = args.locations.resolve();
    let socket = locations.admin_socket();
    if let Ok(mut client) = client::connect(&socket).await
        && let Ok(status) = client.get_status(pb::GetStatusRequest {}).await
    {
        let status = status.into_inner();
        println!("muru: the daemon is already running (pid {})", status.pid);
        return Ok(());
    }

    std::fs::create_dir_all(&locations.logs.path)
        .with_context(|| format!("creating {}", locations.logs.path.display()))?;
    std::fs::create_dir_all(&locations.run.path)
        .with_context(|| format!("creating {}", locations.run.path.display()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(locations.daemon_log())
        .with_context(|| format!("opening {}", locations.daemon_log().display()))?;

    // Its own session, so it survives this shell and its signals.

    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("daemon").arg("run");

    if let Some(listen) = args.listen {
        command.arg("--listen").arg(listen.to_string());
    }

    if let Some(metrics) = args.metrics {
        command.arg("--metrics").arg(metrics.to_string());
    }

    if let Some(dashboard) = args.dashboard {
        command.arg("--dashboard").arg(dashboard.to_string());
    }

    if let Some(user) = &args.user {
        command.arg("--user").arg(user);
    }

    command.args(args.locations.as_arguments());
    command
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);

    // SAFETY: setsid is async-signal-safe and the closure runs in the
    // child between fork and exec, where it touches nothing else.

    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let child = command.spawn().context("starting the daemon")?;
    std::fs::write(locations.pid_file(), child.id().to_string())?;

    let started = Instant::now();

    while started.elapsed() < Duration::from_secs(30) {
        if let Ok(mut client) = client::connect(&socket).await
            && let Ok(status) = client.get_status(pb::GetStatusRequest {}).await
        {
            let status = status.into_inner();
            println!(
                "muru: daemon started (pid {}), serving on {}, log {}",
                status.pid,
                status.listeners.join(" and "),
                locations.daemon_log().display()
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!(
        "the daemon did not answer within 30 seconds; see {}",
        locations.daemon_log().display()
    )
}

/// How long one exchange with a daemon that is being stopped may take
/// before it counts as not answering.
const STOP_EXCHANGE: Duration = Duration::from_secs(5);

/// Ask the daemon at `path` for its status, within `bound`. None when
/// there is no daemon there, it refused, or it did not answer in time.
async fn status_within(path: &Path, bound: Duration) -> Option<pb::Status> {
    let exchange = async {
        let mut client = client::connect(path).await.ok()?;
        client
            .get_status(pb::GetStatusRequest {})
            .await
            .ok()
            .map(|status| status.into_inner())
    };

    tokio::time::timeout(bound, exchange).await.ok().flatten()
}

/// Stop the daemon through its API and wait for its process to go.
///
/// Every exchange with the daemon is bounded, so a daemon that accepts a
/// connection and then never answers, or one that lingers after it has
/// logged its exit, cannot hold the command past `timeout`.
pub async fn daemon_stop(
    socket: SocketArgs,
    shutdown: bool,
    timeout: Duration,
) -> anyhow::Result<()> {
    let path = socket.socket();
    let started = Instant::now();
    let bound = timeout.min(STOP_EXCHANGE);
    let connected = tokio::time::timeout(bound, client::connect(&path)).await;

    let mut client = match connected {
        Ok(client) => client?,
        Err(_) => anyhow::bail!(
            "the daemon at {} did not complete a connection within {} seconds",
            path.display(),
            bound.as_secs()
        ),
    };

    let status = tokio::time::timeout(bound, client.get_status(pb::GetStatusRequest {}))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "the daemon at {} did not answer within {} seconds",
                path.display(),
                bound.as_secs()
            )
        })??
        .into_inner();

    // The daemon may close the connection as it exits, or take its time
    // answering; whether it did exit is checked below either way.

    let _ = tokio::time::timeout(
        bound,
        client.shutdown(pb::ShutdownRequest { drain: shutdown }),
    )
    .await;

    // The daemon removes its socket as it exits; a process that has exited
    // but not been reaped would still answer kill(0), so the socket is the
    // signal to wait for.

    while started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        let answering = status_within(&path, remaining.min(bound)).await.is_some();

        if !answering && (!path.exists() || !process_alive(status.pid)) {
            if shutdown {
                println!("muru: daemon stopped; releases drained and stopped");
            } else {
                println!("muru: daemon detached; hosts and the edge keep serving");
            }
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!(
        "the daemon (pid {}) did not exit within {} seconds; see {}",
        status.pid,
        timeout.as_secs(),
        socket.locations.resolve().daemon_log().display()
    )
}

/// Re-execute the daemon from the installed binary and wait until it
/// answers again with the edge replaced.
pub async fn daemon_upgrade(socket: SocketArgs, timeout: Duration) -> anyhow::Result<()> {
    let path = socket.socket();
    let mut client = client::connect(&path).await?;
    let before = client
        .get_status(pb::GetStatusRequest {})
        .await?
        .into_inner();
    let response = client
        .upgrade_daemon(pb::UpgradeDaemonRequest {})
        .await
        .map_err(|status| anyhow::anyhow!("{}", status.message()))?
        .into_inner();
    println!(
        "muru: daemon (pid {}, version {}) re-executing from {} (version {})",
        before.pid, before.version, response.executable, response.version
    );

    // The daemon answers again from the new image with a new start time,
    // re-adopts the hosts, and then replaces the edge.

    let started = Instant::now();
    let mut latest = None;

    while started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        let status = status_within(&path, remaining.min(STOP_EXCHANGE)).await;

        if let Some(status) = status {
            if status.started_unix_ms != before.started_unix_ms
                && status.edge_pid != 0
                && status.edge_pid != before.edge_pid
            {
                println!(
                    "muru: daemon upgraded to version {} (pid {}); edge pid {} replaced by pid {}",
                    status.version, status.pid, before.edge_pid, status.edge_pid
                );
                return Ok(());
            }

            latest = Some(status);
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    match latest {
        Some(status) if status.started_unix_ms != before.started_unix_ms => anyhow::bail!(
            "the daemon re-executed (pid {}, version {}) but the edge was not replaced; \
             see muructl events",
            status.pid,
            status.version
        ),
        _ => anyhow::bail!(
            "the daemon (pid {}) did not answer again within {} seconds; see {}",
            before.pid,
            timeout.as_secs(),
            socket.locations.resolve().daemon_log().display()
        ),
    }
}

/// What `debug` takes: a release or an application (its current
/// release), one worker or all, and the output form.
#[derive(Args)]
pub struct DebugArgs {
    /// The release as app@release, or an application for its current release.
    target: String,

    /// One worker, by index, instead of every worker of the release.
    #[arg(long, value_name = "N")]
    worker: Option<u32>,

    /// Print each worker's answer as JSON instead of text.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    socket: SocketArgs,
}

/// The debug subcommands (section 11.7).
#[derive(Subcommand)]
pub enum DebugCommand {
    /// Each worker's pid, uptime, requests served and in flight, the age
    /// of its oldest request and its memory, from the host.
    Status(DebugArgs),

    /// The Python stack of every thread in every worker, with the request
    /// each thread is serving, merged where workers show the same stack.
    Stacks(DebugArgs),

    /// The asyncio tasks of each ASGI worker's event loop, what each
    /// awaits, and the loop's lag.
    Tasks(DebugArgs),

    /// Each worker's interpreter, sys.path, loaded modules, threads,
    /// descriptors and environment (secrets redacted).
    Inspect(DebugArgs),

    /// Sample every thread's stack for a period, merged across the
    /// workers, as collapsed stacks for a flame graph.
    Profile(ProfileArgs),

    /// Count calls and time every Python function with sys.monitoring
    /// for a period (Python 3.12 or later); refused on a busy worker
    /// unless forced.
    Trace(TraceArgs),

    /// Each worker's memory, gc state and most common object types, and
    /// with --trace the sites that allocated most over a period.
    Memory(MemoryArgs),

    /// Run Python code in the workers and print its output and value;
    /// allowed only when the daemon's [daemon] table sets debug_exec.
    Exec(ExecArgs),

    /// Open a pdb session in a worker: on the thread stopped at
    /// breakpoint(), else on a thread of its own. Each line read is a pdb
    /// command, and the end of input lets the code go on. Without
    /// --worker, the first worker with a breakpoint waiting. Allowed only
    /// when the daemon's [daemon] table sets debug_exec.
    Attach(AttachArgs),
}

/// `muructl debug attach`.
#[derive(Args)]
pub struct AttachArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// Carry the Debug Adapter Protocol for an IDE instead of a pdb
    /// session, to debugpy in the worker, which the release's
    /// environment must have.
    #[arg(long)]
    dap: bool,

    /// Where the IDE connects with --dap.
    #[arg(long, value_name = "ADDRESS", default_value = "127.0.0.1:5678")]
    listen: SocketAddr,
}

/// `muructl debug profile`.
#[derive(Args)]
pub struct ProfileArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// How long to sample.
    #[arg(long = "for", value_name = "DURATION", default_value = "10s", value_parser = muru_config::parse_duration)]
    duration: Duration,

    /// How often to sample.
    #[arg(long, value_name = "DURATION", default_value = "10ms", value_parser = muru_config::parse_duration)]
    interval: Duration,

    /// Write the collapsed stacks here rather than to standard output.
    #[arg(long, short, value_name = "FILE")]
    output: Option<PathBuf>,
}

/// `muructl debug trace`.
#[derive(Args)]
pub struct TraceArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// How long to trace.
    #[arg(long = "for", value_name = "DURATION", default_value = "10s", value_parser = muru_config::parse_duration)]
    duration: Duration,

    /// Trace a worker serving requests in more than half its slots.
    #[arg(long)]
    force: bool,

    /// How many functions to show, by time.
    #[arg(long, value_name = "N", default_value_t = 30)]
    top: u32,
}

/// `muructl debug memory`.
#[derive(Args)]
pub struct MemoryArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// Trace allocations with tracemalloc for the period.
    #[arg(long)]
    trace: bool,

    /// How long to trace allocations.
    #[arg(long = "for", value_name = "DURATION", default_value = "30s", value_parser = muru_config::parse_duration)]
    duration: Duration,

    /// How many object types and allocation sites to show.
    #[arg(long, value_name = "N", default_value_t = 20)]
    top: u32,
}

/// `muructl debug exec`.
#[derive(Args)]
pub struct ExecArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// How long the code may run.
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = muru_config::parse_duration)]
    timeout: Duration,

    /// The code, after --.
    #[arg(last = true, required = true, value_name = "CODE")]
    code: Vec<String>,
}

/// The logging subcommands (section 11.4).
#[derive(Subcommand)]
pub enum LoggingCommand {
    /// Set loggers' levels in a release's workers, for a period or until reset.
    Level {
        /// The release as app@release, or an application for its current release.
        target: String,

        /// A logger and its level as NAME=LEVEL (root with an empty name,
        /// as =DEBUG). Repeatable.
        #[arg(value_name = "NAME=LEVEL", required_unless_present = "reset")]
        levels: Vec<String>,

        /// Revert on its own after this long, for example 10m.
        #[arg(long = "for", value_name = "DURATION", value_parser = muru_config::parse_duration)]
        duration: Option<Duration>,

        /// Put every level changed this way back.
        #[arg(long)]
        reset: bool,

        /// One worker, by index, instead of every worker of the release.
        #[arg(long, value_name = "N")]
        worker: Option<u32>,

        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// Ask a release's workers something and print what came back.
pub async fn debug(command: DebugCommand) -> anyhow::Result<()> {
    if let DebugCommand::Attach(args) = &command {
        return attach(args).await;
    }

    // Section 11.7: an operation over a period gives the host that long
    // and a little more before it gives up on a worker.

    let period = |duration: Duration| duration.as_secs_f64() + 5.0;

    let (op, args, request) = match &command {
        DebugCommand::Status(args) => ("status", args, serde_json::json!({})),
        DebugCommand::Stacks(args) => ("stacks", args, serde_json::json!({})),
        DebugCommand::Tasks(args) => ("tasks", args, serde_json::json!({})),
        DebugCommand::Inspect(args) => ("inspect", args, serde_json::json!({})),
        DebugCommand::Profile(profile) => (
            "profile",
            &profile.common,
            serde_json::json!({
                "duration_s": profile.duration.as_secs_f64(),
                "interval_ms": profile.interval.as_secs_f64() * 1000.0,
                "timeout_s": period(profile.duration),
            }),
        ),
        DebugCommand::Trace(trace) => (
            "trace",
            &trace.common,
            serde_json::json!({
                "duration_s": trace.duration.as_secs_f64(),
                "force": trace.force,
                "top": trace.top,
                "timeout_s": period(trace.duration),
            }),
        ),
        DebugCommand::Memory(memory) => (
            "memory",
            &memory.common,
            serde_json::json!({
                "trace": memory.trace,
                "duration_s": memory.duration.as_secs_f64(),
                "top": memory.top,
                "timeout_s": if memory.trace { period(memory.duration) } else { 30.0 },
            }),
        ),
        DebugCommand::Exec(exec) => (
            "exec",
            &exec.common,
            serde_json::json!({
                "code": exec.code.join(" "),
                "timeout_s": exec.timeout.as_secs_f64(),
            }),
        ),
        DebugCommand::Attach(_) => unreachable!("attach is handled above"),
    };

    let response = ask(&args.socket, &args.target, op, args.worker, request).await?;

    if args.json {
        print_json(&response);
        return Ok(());
    }

    match &command {
        DebugCommand::Status(_) => print_status(&response),
        DebugCommand::Stacks(_) => print_stacks(&response),
        DebugCommand::Tasks(_) => print_tasks(&response),
        DebugCommand::Inspect(_) => print_inspect(&response),
        DebugCommand::Profile(profile) => write_profile(&response, profile.output.as_deref())?,
        DebugCommand::Trace(_) => print_trace(&response),
        DebugCommand::Memory(_) => print_memory(&response),
        DebugCommand::Exec(_) => print_exec(&response),
        DebugCommand::Attach(_) => {}
    }

    Ok(())
}

/// How often a session with nothing typed asks for pdb's output.
const ATTACH_POLL: Duration = Duration::from_millis(300);

/// `muructl debug attach` (section 11.7): a pdb session carried line by
/// line, each line read going to the worker with a request whose answer
/// brings what pdb wrote since, and the session polled while nothing is
/// typed.
async fn attach(attach_args: &AttachArgs) -> anyhow::Result<()> {
    use std::io::Write as _;

    let args = &attach_args.common;

    let worker = match args.worker {
        Some(worker) => worker,
        None => {
            let response = ask(
                &args.socket,
                &args.target,
                "breakpoints",
                None,
                serde_json::json!({}),
            )
            .await?;
            let waiting = response.workers.iter().find(|answer| {
                parse_result(answer).is_some_and(|result| {
                    result["waiting"]
                        .as_array()
                        .is_some_and(|waiting| !waiting.is_empty())
                })
            });

            waiting
                .or(response.workers.first())
                .map_or(0, |answer| answer.index)
        }
    };

    let exchange = |op: &'static str, request: serde_json::Value| async move {
        let response = ask(&args.socket, &args.target, op, Some(worker), request).await?;
        let answer = response
            .workers
            .first()
            .ok_or_else(|| anyhow::anyhow!("worker {worker} did not answer"))?;

        parse_result(answer).ok_or_else(|| anyhow::anyhow!("worker {worker}: {}", answer.error))
    };

    if attach_args.dap {
        return relay_dap(attach_args.listen, worker, exchange).await;
    }

    let opened = exchange("attach", serde_json::json!({})).await?;
    let session = opened["session"].as_u64().unwrap_or_default();
    eprintln!(
        "muru: attached to worker {worker} at {}; the end of input lets the code go on",
        opened["where"].as_str().unwrap_or("?")
    );
    print!("{}", opened["output"].as_str().unwrap_or_default());
    std::io::stdout().flush()?;

    // Lines are read on a thread of their own, since reading blocks.

    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();

    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else {
                break;
            };

            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        let (op, request) = match tokio::time::timeout(ATTACH_POLL, lines.recv()).await {
            Ok(Some(line)) => (
                "attach_io",
                serde_json::json!({"session": session, "input": line, "wait_s": 0.5}),
            ),
            Ok(None) => ("detach", serde_json::json!({"session": session})),
            Err(_) => (
                "attach_io",
                serde_json::json!({"session": session, "input": "", "wait_s": 0.0}),
            ),
        };

        let result = exchange(op, request).await?;
        print!("{}", result["output"].as_str().unwrap_or_default());
        std::io::stdout().flush()?;

        if op == "detach" || result["ended"].as_bool().unwrap_or(true) {
            eprintln!("muru: the session in worker {worker} has ended");
            return Ok(());
        }
    }
}

/// `muructl debug attach --dap` (section 11.7): the Debug Adapter
/// Protocol between an IDE connected to `listen` and debugpy's adapter in
/// the worker, what the IDE sent going with each request and what the
/// adapter sent coming back with its answer, base64, and the session
/// polled while the IDE is quiet. The IDE closing its connection ends it.
async fn relay_dap<F, R>(listen: SocketAddr, worker: u32, exchange: F) -> anyhow::Result<()>
where
    F: Fn(&'static str, serde_json::Value) -> R,
    R: std::future::Future<Output = anyhow::Result<serde_json::Value>>,
{
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    eprintln!(
        "muru: waiting for an IDE to attach to {} with the Debug Adapter Protocol",
        listener.local_addr()?
    );
    let (ide, _) = listener.accept().await?;
    drop(listener);

    let opened = exchange("attach", serde_json::json!({"dap": true})).await?;
    let session = opened["session"].as_u64().unwrap_or_default();
    eprintln!(
        "muru: attached to worker {worker} at {}; the IDE disconnecting lets the code go on",
        opened["where"].as_str().unwrap_or("?")
    );

    let (mut from_ide, mut to_ide) = ide.into_split();
    let mut buffer = vec![0; 65536];

    loop {
        let (op, request) =
            match tokio::time::timeout(ATTACH_POLL, from_ide.read(&mut buffer)).await {
                Ok(Ok(0) | Err(_)) => ("detach", serde_json::json!({"session": session})),
                Ok(Ok(read)) => (
                    "attach_io",
                    serde_json::json!({
                        "session": session,
                        "data": STANDARD.encode(&buffer[..read]),
                        "wait_s": 0.5,
                    }),
                ),
                Err(_) => (
                    "attach_io",
                    serde_json::json!({"session": session, "data": "", "wait_s": 0.0}),
                ),
            };

        let result = exchange(op, request).await?;
        let sent = STANDARD
            .decode(result["data"].as_str().unwrap_or_default())
            .context("the worker's answer")?;

        // An IDE that has gone ends the session at the next turn.

        if !sent.is_empty() && to_ide.write_all(&sent).await.is_err() {
            exchange("detach", serde_json::json!({"session": session})).await?;
            eprintln!("muru: the session in worker {worker} has ended");
            return Ok(());
        }

        if op == "detach" || result["ended"].as_bool().unwrap_or(true) {
            eprintln!("muru: the session in worker {worker} has ended");
            return Ok(());
        }
    }
}

/// Merge the workers' collapsed stacks and write them, heaviest first,
/// with the functions that took most samples on standard error.
fn write_profile(response: &pb::DebugResponse, output: Option<&Path>) -> anyhow::Result<()> {
    use std::io::Write as _;

    let mut stacks: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut leaves: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut rounds = 0;

    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            eprintln!(
                "worker {} (pid {}): {}",
                answer.index, answer.pid, answer.error
            );
            continue;
        };

        rounds += result["rounds"].as_u64().unwrap_or(0);

        for (stack, count) in result["stacks"].as_object().into_iter().flatten() {
            let count = count.as_u64().unwrap_or(0);
            *stacks.entry(stack.clone()).or_default() += count;

            if let Some(leaf) = stack.rsplit(';').next() {
                *leaves.entry(leaf.to_owned()).or_default() += count;
            }
        }
    }

    let mut ordered: Vec<(&String, &u64)> = stacks.iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut text = String::new();

    for (stack, count) in ordered {
        text.push_str(&format!("{stack} {count}\n"));
    }

    match output {
        Some(path) => {
            std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?
        }
        None => std::io::stdout().write_all(text.as_bytes())?,
    }

    let total: u64 = leaves.values().sum();
    let mut busiest: Vec<(String, u64)> = leaves.into_iter().collect();
    busiest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    eprintln!(
        "{total} samples of running Python code in {rounds} rounds across {} worker{}",
        response.workers.len(),
        if response.workers.len() == 1 { "" } else { "s" }
    );

    for (function, count) in busiest.iter().take(10) {
        eprintln!(
            "  {:>5.1}%  {function}",
            *count as f64 * 100.0 / total.max(1) as f64
        );
    }

    if let Some(path) = output {
        eprintln!("muru: collapsed stacks written to {}", path.display());
    }

    Ok(())
}

fn print_trace(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}\n",
                answer.index, answer.pid, answer.error
            );
            continue;
        };

        println!("worker {} (pid {})", answer.index, answer.pid);
        println!("  {:>10}  {:>8}  function", "total ms", "calls");

        for function in result["functions"].as_array().into_iter().flatten() {
            println!(
                "  {:>10.3}  {:>8}  {}",
                function["total_ms"].as_f64().unwrap_or(0.0),
                function["calls"].as_u64().unwrap_or(0),
                function["function"].as_str().unwrap_or("?")
            );
        }

        println!();
    }
}

fn print_memory(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}\n",
                answer.index, answer.pid, answer.error
            );
            continue;
        };

        println!("worker {} (pid {})", answer.index, answer.pid);

        if let Some(memory) = result["memory"].as_object() {
            let size = |name: &str| {
                memory
                    .get(name)
                    .and_then(serde_json::Value::as_u64)
                    .map(muru_config::format_size)
                    .unwrap_or_else(|| "?".to_owned())
            };
            println!(
                "  rss {}  pss {}  uss {}",
                size("rss"),
                size("pss"),
                size("uss")
            );
        }

        println!("  gc counts {}", result["gc"]["counts"]);
        println!("  objects:");

        for entry in result["objects"].as_array().into_iter().flatten() {
            println!(
                "    {:>9}  {}",
                entry[1].as_u64().unwrap_or(0),
                entry[0].as_str().unwrap_or("?")
            );
        }

        if let Some(allocations) = result["allocations"].as_array() {
            println!("  allocated over the period:");

            for site in allocations {
                println!(
                    "    {:>+12}  {:>+8}  {}",
                    site["size_diff"].as_i64().unwrap_or(0),
                    site["count_diff"].as_i64().unwrap_or(0),
                    site["site"].as_str().unwrap_or("?")
                );
            }
        }

        println!();
    }
}

fn print_exec(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}",
                answer.index, answer.pid, answer.error
            );
            continue;
        };

        println!("worker {} (pid {}):", answer.index, answer.pid);
        print!("{}", result["output"].as_str().unwrap_or_default());

        if let Some(value) = result["value"].as_str() {
            println!("{value}");
        }
    }
}

/// Change logger levels in a release's workers.
pub async fn logging(command: LoggingCommand) -> anyhow::Result<()> {
    let LoggingCommand::Level {
        target,
        levels,
        duration,
        reset,
        worker,
        socket,
    } = command;

    if reset {
        let response = ask(
            &socket,
            &target,
            "logging",
            worker,
            serde_json::json!({"reset": true}),
        )
        .await?;

        for answer in &response.workers {
            match parse_result(answer) {
                Some(result) => println!(
                    "worker {} (pid {}): reset {}",
                    answer.index,
                    answer.pid,
                    result["reset"]
                        .as_array()
                        .map(|names| {
                            names
                                .iter()
                                .filter_map(|name| name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| "nothing".to_owned())
                ),
                None => println!(
                    "worker {} (pid {}): {}",
                    answer.index, answer.pid, answer.error
                ),
            }
        }

        return Ok(());
    }

    for setting in levels {
        let (name, level) = setting.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("{setting:?} is not NAME=LEVEL, for example myapp=DEBUG")
        })?;
        let args = serde_json::json!({
            "logger": name,
            "level": level,
            "duration_s": duration.map(|duration| duration.as_secs_f64()),
        });
        let response = ask(&socket, &target, "logging", worker, args).await?;

        for answer in &response.workers {
            match parse_result(answer) {
                Some(result) => println!(
                    "worker {} (pid {}): {} {} (was {}){}",
                    answer.index,
                    answer.pid,
                    result["logger"].as_str().unwrap_or("root"),
                    result["level"].as_str().unwrap_or(level),
                    result["previous"].as_str().unwrap_or("?"),
                    duration
                        .map(|duration| format!(", for {}", muru_config::format_duration(duration)))
                        .unwrap_or_default()
                ),
                None => println!(
                    "worker {} (pid {}): {}",
                    answer.index, answer.pid, answer.error
                ),
            }
        }
    }

    Ok(())
}

/// Send one debug request for a target: an application, or app@release.
async fn ask(
    socket: &SocketArgs,
    target: &str,
    op: &str,
    worker: Option<u32>,
    args: serde_json::Value,
) -> anyhow::Result<pb::DebugResponse> {
    let (app, release) = match target.split_once('@') {
        Some((app, release)) if !app.is_empty() && !release.is_empty() => (app, release),
        Some(_) => anyhow::bail!("{target:?} is not a release: expected app@release"),
        None => (target, ""),
    };

    let mut client = client::connect(&socket.socket()).await?;
    let response = client
        .debug(pb::DebugRequest {
            app: app.to_owned(),
            release: release.to_owned(),
            op: op.to_owned(),
            worker,
            args_json: args.to_string(),
        })
        .await
        .map_err(explain)?
        .into_inner();

    if response.workers.is_empty() {
        anyhow::bail!(
            "{}@{} has no worker to ask{}",
            app,
            response.release,
            worker
                .map(|index| format!(" with index {index}"))
                .unwrap_or_default()
        );
    }

    Ok(response)
}

/// A worker's answer as JSON, when it answered.
fn parse_result(answer: &pb::WorkerDebug) -> Option<serde_json::Value> {
    if !answer.ok {
        return None;
    }

    serde_json::from_str(&answer.result_json).ok()
}

fn print_json(response: &pb::DebugResponse) {
    let workers: Vec<serde_json::Value> = response
        .workers
        .iter()
        .map(|answer| {
            serde_json::json!({
                "index": answer.index,
                "pid": answer.pid,
                "ok": answer.ok,
                "error": (!answer.error.is_empty()).then_some(answer.error.as_str()),
                "result": parse_result(answer),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "release": response.release,
            "workers": workers,
        }))
        .unwrap_or_default()
    );
}

fn print_status(response: &pb::DebugResponse) {
    println!(
        "{:<7} {:<8} {:<10} {:<9} {:<10} {:<10} {:<9} {:<9} {:<9} STATE",
        "WORKER", "PID", "GROUP", "UP", "REQUESTS", "IN-FLIGHT", "OLDEST", "RSS", "USS"
    );

    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!("{:<7} {:<8} {}", answer.index, answer.pid, answer.error);
            continue;
        };
        let size = |key: &str| {
            result[key]
                .as_u64()
                .map(muru_config::format_size_approx)
                .unwrap_or_else(|| "-".to_owned())
        };

        let state = if result["replaced"].as_bool() == Some(true) {
            "replaced"
        } else if result["debugging"].as_bool() == Some(true) {
            "debugging"
        } else if result["ready"].as_bool() == Some(true) {
            "ready"
        } else {
            "starting"
        };
        println!(
            "{:<7} {:<8} {:<10} {:<9} {:<10} {:<10} {:<9} {:<9} {:<9} {state}",
            answer.index,
            answer.pid,
            result["group"].as_str().unwrap_or("default"),
            format!("{:.0}s", result["uptime_s"].as_f64().unwrap_or(0.0)),
            result["requests"].as_u64().unwrap_or(0),
            result["in_flight"].as_u64().unwrap_or(0),
            format!("{}ms", result["oldest_ms"].as_u64().unwrap_or(0)),
            size("rss"),
            size("uss"),
        );
    }
}

fn print_stacks(response: &pb::DebugResponse) {
    // Threads with the same frames across workers are shown once with
    // the workers they came from.

    let mut merged: Vec<(String, Vec<String>, Vec<u32>, serde_json::Value)> = Vec::new();

    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}\n",
                answer.index, answer.pid, answer.error
            );
            continue;
        };

        for thread in result["threads"].as_array().into_iter().flatten() {
            let name = thread["name"].as_str().unwrap_or("?").to_owned();
            let frames: Vec<String> = thread["frames"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|frame| {
                    format!(
                        "  File \"{}\", line {}, in {}\n    {}",
                        frame["file"].as_str().unwrap_or("?"),
                        frame["line"].as_u64().unwrap_or(0),
                        frame["function"].as_str().unwrap_or("?"),
                        frame["text"].as_str().unwrap_or("").trim()
                    )
                })
                .collect();
            let request = thread["request"].clone();
            let same = merged
                .iter_mut()
                .find(|(other, other_frames, _, other_request)| {
                    *other == name
                        && *other_frames == frames
                        && other_request.is_null()
                        && request.is_null()
                });

            match same {
                Some(entry) => entry.2.push(answer.index),
                None => merged.push((name, frames, vec![answer.index], request)),
            }
        }
    }

    for (name, frames, workers, request) in merged {
        let where_ = if workers.len() == 1 {
            format!("worker {}", workers[0])
        } else {
            format!(
                "{} workers ({})",
                workers.len(),
                workers
                    .iter()
                    .map(|index| index.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        println!("{name} in {where_}:");

        if !request.is_null() {
            println!(
                "  serving {} {} for {:.1} s (request {})",
                request["method"].as_str().unwrap_or("?"),
                request["path"].as_str().unwrap_or("?"),
                request["elapsed_ms"].as_f64().unwrap_or(0.0) / 1000.0,
                request["request_id"].as_str().unwrap_or("?")
            );
        }

        if frames.is_empty() {
            println!("  (no Python frames)");
        }

        for frame in frames {
            println!("{frame}");
        }

        println!();
    }
}

fn print_tasks(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}",
                answer.index, answer.pid, answer.error
            );
            continue;
        };

        match result["lag_ms"].as_f64() {
            Some(lag) => println!(
                "worker {} (pid {}): loop lag {lag:.1} ms",
                answer.index, answer.pid
            ),
            None => {
                println!(
                    "worker {} (pid {}): {}",
                    answer.index,
                    answer.pid,
                    result["note"].as_str().unwrap_or("no event loop")
                );
                continue;
            }
        }

        let tasks = result["tasks"].as_array().cloned().unwrap_or_default();

        if tasks.is_empty() {
            println!("  no tasks");
        }

        for task in tasks {
            let at = task["at"]
                .as_object()
                .map(|at| {
                    format!(
                        " at {}:{}",
                        at["file"].as_str().unwrap_or("?"),
                        at["line"].as_u64().unwrap_or(0)
                    )
                })
                .unwrap_or_default();
            println!(
                "  {} {}{}{}",
                task["name"].as_str().unwrap_or("?"),
                task["coro"].as_str().unwrap_or("?"),
                task["waiting_on"]
                    .as_str()
                    .map(|waiting| format!(" waiting on {waiting}"))
                    .unwrap_or_default(),
                at
            );
        }

        println!();
    }
}

fn print_inspect(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}",
                answer.index, answer.pid, answer.error
            );
            continue;
        };
        let settings = &result["settings"];
        println!(
            "worker {} (pid {}): python {} at {}, {} in {} (group {}, {}{})",
            answer.index,
            answer.pid,
            result["python"].as_str().unwrap_or("?"),
            result["executable"].as_str().unwrap_or("?"),
            settings["app"].as_str().unwrap_or("?"),
            result["cwd"].as_str().unwrap_or("?"),
            settings["group"].as_str().unwrap_or("default"),
            settings["process_model"].as_str().unwrap_or("?"),
            if settings["gil_enabled"].as_bool() == Some(false) {
                ", free-threaded"
            } else {
                ""
            }
        );
        println!("  sys.path:");

        for entry in result["sys_path"].as_array().into_iter().flatten() {
            println!("    {}", entry.as_str().unwrap_or("?"));
        }

        let modules = result["modules"].as_object().cloned().unwrap_or_default();
        let versioned: Vec<String> = modules
            .iter()
            .filter_map(|(name, module)| {
                module["version"]
                    .as_str()
                    .map(|version| format!("{name} {version}"))
            })
            .collect();
        println!(
            "  {} modules loaded{}{}",
            modules.len(),
            if versioned.is_empty() {
                ""
            } else {
                ", with versions: "
            },
            versioned.join(", ")
        );
        let threads: Vec<String> = result["threads"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|thread| thread["name"].as_str().unwrap_or("?").to_owned())
            .collect();
        println!("  threads: {}", threads.join(", "));

        if let Some(count) = result["descriptors"]["count"].as_u64() {
            println!("  {count} open descriptors");
        }

        println!("  environment:");

        for (key, value) in result["environment"].as_object().into_iter().flatten() {
            println!("    {key}={}", value.as_str().unwrap_or(""));
        }

        println!();
    }
}

/// Print the node's metrics as Prometheus text.
pub async fn metrics(socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let text = client
        .get_metrics(pb::GetMetricsRequest {})
        .await?
        .into_inner()
        .text;
    print!("{text}");
    Ok(())
}

/// Re-read the daemon's configuration file.
pub async fn daemon_reload(socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let response = client
        .reload_config(pb::ReloadConfigRequest {})
        .await
        .map_err(|status| anyhow::anyhow!("{}", status.message()))?
        .into_inner();
    println!("muru: {}", response.message);
    Ok(())
}

/// Show the daemon's status.
pub async fn daemon_status(socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let status = client
        .get_status(pb::GetStatusRequest {})
        .await?
        .into_inner();

    println!("daemon   pid {} version {}", status.pid, status.version);
    println!("edge     pid {}", status.edge_pid);
    println!("serving  {}", status.listeners.join(", "));
    println!("socket   {}", status.socket);

    if !status.metrics.is_empty() {
        println!("metrics  http://{}/metrics", status.metrics);
    }

    if !status.dashboard.is_empty() {
        println!("dashboard http://{}/", status.dashboard);
    }

    println!("state    {}", status.state_dir);
    println!("run      {}", status.run_dir);
    println!("apps     {} ({} releases)", status.apps, status.releases);

    if !status.budget.is_empty() {
        println!("budget   {}", status.budget);
    }

    Ok(())
}

/// Make sure a daemon runs: start one when none answers.
pub async fn up(args: DaemonRunArgs) -> anyhow::Result<()> {
    daemon_start(args).await
}

/// What `deploy` takes.
#[derive(Args)]
pub struct DeployArgs {
    /// The application name.
    app: String,

    /// The source directory (default: the current directory).
    source: Option<PathBuf>,

    /// The application as module:callable, else from the source's [tool.muru] table.
    #[arg(long)]
    entrypoint: Option<String>,

    /// The interface the entrypoint speaks: auto, wsgi or asgi.
    #[arg(long, value_name = "NAME")]
    interface: Option<muru_config::Interface>,

    /// The interpreter (default: the source's .venv, else the previous release's).
    #[arg(long)]
    python: Option<PathBuf>,

    /// Worker processes.
    #[arg(long)]
    processes: Option<u32>,

    /// Application threads per worker.
    #[arg(long)]
    threads: Option<u32>,

    /// Threads for synchronous code called from an ASGI application.
    #[arg(long, value_name = "N")]
    sync_threads: Option<u32>,

    /// Requests an ASGI worker's event loop takes at once.
    #[arg(long, value_name = "N")]
    concurrency: Option<u32>,

    /// The preload mode: auto, true or false.
    #[arg(long, value_name = "MODE")]
    preload: Option<muru_config::Preload>,

    /// The hooks module.
    #[arg(long)]
    hooks: Option<String>,

    /// The entrypoint is a factory returning the application.
    #[arg(long)]
    factory: bool,

    /// The release id (default: generated, r1, r2, ...).
    #[arg(long, value_name = "ID")]
    release: Option<String>,

    /// Return as soon as the release is recorded, without waiting for it
    /// to serve.
    #[arg(long)]
    no_wait: bool,

    /// Start the release but leave traffic where it is, for a preview
    /// with the Muru-Preview header or a rollout (section 9.2).
    #[arg(long)]
    hold: bool,

    /// Deploy although an overlay freezes the application's deploys
    /// (section 9.6); recorded as an event.
    #[arg(long)]
    override_freeze: bool,

    /// How long the release this one replaces stays warm for an instant
    /// rollback (default 30m; 0 stops it as soon as it has drained).
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    retain: Option<Duration>,

    /// The workers each group of the release this one replaces keeps
    /// while it stays warm (default 1; 0 keeps the host alone).
    #[arg(long, value_name = "N")]
    retain_workers: Option<u32>,

    /// Replace a worker after this many requests, with jitter.
    #[arg(long, value_name = "N")]
    max_requests: Option<u64>,

    /// Replace a worker after this long, for example 1h, with jitter.
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    max_lifetime: Option<Duration>,

    /// A host timeout as NAME=DURATION: soft (interrupt a request that
    /// runs this long; off), hard (replace a worker whose request runs
    /// this long; off), hard_drain_grace (10s), shutdown (5s) or
    /// loop_stall (replace an ASGI worker whose event loop stalls this
    /// long; 10s). Repeatable.
    #[arg(long = "timeout", value_name = "NAME=DURATION", value_parser = parse_host_timeout)]
    timeouts: Vec<(String, Duration)>,

    /// A memory setting as KEY=VALUE: measure (uss, pss or rss),
    /// soft_limit (for example 800MB), soft_for (2m), reclaim_first
    /// (true), hard_limit (1200MB) or check_interval (5s). Repeatable.
    #[arg(long = "memory", value_name = "KEY=VALUE", value_parser = crate::parse_memory_setting)]
    memory: Vec<(String, String)>,

    /// An environment variable for the workers as NAME=VALUE, over the
    /// source's [tool.muru.env] table. Repeatable.
    #[arg(long = "env", value_name = "NAME=VALUE", value_parser = parse_env_setting)]
    env: Vec<(String, String)>,

    /// A secret for the workers as NAME=STORE:KEY, delivered as the
    /// variable, or NAME=STORE:KEY:file for a file named by NAME_FILE
    /// (section 10.8). Repeatable.
    #[arg(long = "secret", value_name = "NAME=STORE:KEY[:file]", value_parser = parse_secret_setting)]
    secrets: Vec<(String, String, String, bool)>,

    /// Run an HTTP server of this profile instead of workers (section
    /// 9.9): gunicorn, uvicorn, granian, mod_wsgi-express or generic,
    /// with its command after `--`.
    #[arg(long, value_name = "PROFILE")]
    server: Option<muru_config::ServerProfile>,

    /// How a managed server's instances bind: unix (the default) or port.
    #[arg(long, value_name = "MODE")]
    bind: Option<muru_config::ServerBind>,

    /// How many instances of a managed server's command run (default 1).
    #[arg(long, value_name = "N")]
    instances: Option<u32>,

    /// The path probed until a managed server's instance answers 2xx or
    /// 3xx; any answer makes it ready without one.
    #[arg(long, value_name = "PATH")]
    readiness: Option<String>,

    /// How long a managed server's instances get to be ready (default 60s).
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    readiness_timeout: Option<Duration>,

    /// The signal that stops a managed server's instance gracefully
    /// (TERM, INT, QUIT, HUP, USR1, USR2 or WINCH); required by the
    /// generic profile, the other profiles know their server's.
    #[arg(long, value_name = "NAME")]
    stop_signal: Option<String>,

    /// A directory a path in an X-Muru-Sendfile response header must lie
    /// in for the edge to serve the file; repeatable (default the table's).
    #[arg(long = "sendfile-root", value_name = "DIR")]
    sendfile_roots: Vec<String>,

    /// A second response header honoured as X-Muru-Sendfile is, such as
    /// X-Sendfile.
    #[arg(long, value_name = "NAME")]
    sendfile_header: Option<String>,

    #[command(flatten)]
    socket: SocketArgs,

    /// The command of a managed server, after `--`, with `{bind}` where
    /// it takes its address; without `{bind}` the profile adds the
    /// server's own flags.
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
}

/// Parse NAME=VALUE for --env.
fn parse_env_setting(text: &str) -> Result<(String, String), String> {
    let (name, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=VALUE"))?;

    if !muru_controller::model::valid_env_name(name) {
        return Err(format!("{name:?} is not a variable name"));
    }

    Ok((name.to_owned(), value.to_owned()))
}

/// Parse NAME=STORE:KEY[:file] for --secret.
fn parse_secret_setting(text: &str) -> Result<(String, String, String, bool), String> {
    let (name, reference) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=STORE:KEY"))?;

    if !muru_controller::model::valid_env_name(name) {
        return Err(format!("{name:?} is not a variable name"));
    }

    let (store, rest) = reference
        .split_once(':')
        .ok_or_else(|| format!("{reference:?} is not STORE:KEY"))?;

    let (key, as_file) = match rest.rsplit_once(':') {
        Some((key, "file")) => (key, true),
        Some((key, "env")) => (key, false),
        _ => (rest, false),
    };

    if store.is_empty() || key.is_empty() {
        return Err(format!("{reference:?} is not STORE:KEY"));
    }

    Ok((name.to_owned(), store.to_owned(), key.to_owned(), as_file))
}

/// The secret subcommands (section 10.8): the file stores the daemon's
/// configuration declares, managed without the daemon.
#[derive(Subcommand)]
pub enum SecretCommand {
    /// Store a value read from standard input under a key.
    Set {
        /// The file store, as [secrets.store.NAME] names it.
        store: String,

        /// The key.
        key: String,

        #[command(flatten)]
        locations: LocationArgs,
    },

    /// List the keys of a file store; values are never shown.
    List {
        /// The file store.
        store: String,

        #[command(flatten)]
        locations: LocationArgs,
    },

    /// Remove a key from a file store.
    Remove {
        /// The file store.
        store: String,

        /// The key.
        key: String,

        #[command(flatten)]
        locations: LocationArgs,
    },
}

/// Manage a file secret store.
pub async fn secret(command: SecretCommand) -> anyhow::Result<()> {
    let (store, locations) = match &command {
        SecretCommand::Set {
            store, locations, ..
        }
        | SecretCommand::List { store, locations }
        | SecretCommand::Remove {
            store, locations, ..
        } => (store.clone(), locations.resolve()),
    };

    let document = read_config(&locations.config.path)?;
    let stores = muru_controller::secrets::Store::from_document(
        &document,
        &locations.state.path.join("secrets"),
    )?;

    let directory = match stores.iter().find(|candidate| candidate.name == store) {
        Some(muru_controller::secrets::Store {
            kind: muru_controller::secrets::Kind::File(directory),
            ..
        }) => directory.clone(),
        Some(_) => anyhow::bail!("secret store {store} is not a file store"),
        None => anyhow::bail!(
            "no secret store named {store} in {}",
            locations.config.path.display()
        ),
    };

    match command {
        SecretCommand::Set { key, .. } => {
            anyhow::ensure!(
                !key.is_empty() && !key.contains('/') && !key.starts_with('.'),
                "{key:?} is not a valid key"
            );
            let mut value = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut value)
                .context("reading the value from standard input")?;
            let value = value.trim_end_matches(['\n', '\r']);
            anyhow::ensure!(!value.is_empty(), "no value was given on standard input");
            std::fs::create_dir_all(&directory)
                .with_context(|| format!("creating {}", directory.display()))?;
            std::fs::set_permissions(
                &directory,
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )?;
            let path = directory.join(&key);
            let _ = std::fs::remove_file(&path);
            {
                use std::io::Write as _;
                use std::os::unix::fs::OpenOptionsExt as _;

                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .with_context(|| format!("writing {}", path.display()))?;
                file.write_all(value.as_bytes())?;
            }
            println!("muru: secret {store}/{key} set");
        }
        SecretCommand::List { .. } => {
            let mut keys: Vec<String> = match std::fs::read_dir(&directory) {
                Ok(entries) => entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .filter(|name| !name.starts_with('.'))
                    .collect(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => {
                    return Err(error).context(format!("reading {}", directory.display()));
                }
            };

            keys.sort();

            if keys.is_empty() {
                println!("no secrets in {store}");
            }

            for key in keys {
                println!("{key}");
            }
        }
        SecretCommand::Remove { key, .. } => {
            let path = directory.join(&key);
            std::fs::remove_file(&path)
                .with_context(|| format!("secret {store}/{key} does not exist"))?;
            println!("muru: secret {store}/{key} removed");
        }
    }

    Ok(())
}

/// The last value given for a memory setting.
fn memory_setting(settings: &[(String, String)], key: &str) -> Option<String> {
    settings
        .iter()
        .rev()
        .find(|(given, _)| given == key)
        .map(|(_, value)| value.clone())
}

/// The last value given for a named timeout, in milliseconds.
fn timeout_ms(timeouts: &[(String, Duration)], name: &str) -> Option<u64> {
    timeouts
        .iter()
        .rev()
        .find(|(given, _)| given == name)
        .map(|(_, duration)| duration.as_millis() as u64)
}

/// Parse NAME=DURATION for deploy --timeout.
fn parse_host_timeout(text: &str) -> Result<(String, Duration), String> {
    let (name, duration) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=DURATION, for example hard=60s"))?;
    let name = name.trim();

    if !["soft", "hard", "hard_drain_grace", "shutdown", "loop_stall"].contains(&name) {
        return Err(format!(
            "{name:?} is not a host timeout name: soft, hard, hard_drain_grace, shutdown or \
             loop_stall"
        ));
    }

    Ok((name.to_owned(), muru_config::parse_duration(duration)?))
}

/// An interpreter as the daemon should see it: a path with a directory in
/// it is made absolute against `base` (else the working directory), since
/// the daemon runs elsewhere; a bare command name is left for the PATH.
fn absolute_interpreter(python: &Path, base: Option<&Path>) -> String {
    if python.is_absolute() || python.components().count() < 2 {
        return python.display().to_string();
    }

    let base = base
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join(python).display().to_string()
}

/// Deploy a release.
pub async fn deploy(args: DeployArgs) -> anyhow::Result<()> {
    let source = args
        .source
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let source = source
        .canonicalize()
        .with_context(|| format!("{} does not exist", source.display()))?;

    if args.server.is_none() && !args.command.is_empty() {
        anyhow::bail!("a command after -- needs --server PROFILE");
    }

    let mut client = client::connect(&args.socket.socket()).await?;

    let request = pb::DeployRequest {
        app: args.app.clone(),
        source: source.display().to_string(),
        kind: args.server.map(|_| "http".to_owned()).unwrap_or_default(),
        profile: args
            .server
            .map(|profile| profile.as_str().to_owned())
            .unwrap_or_default(),
        command: args.command.clone(),
        bind: args
            .bind
            .map(|bind| bind.as_str().to_owned())
            .unwrap_or_default(),
        instances: args.instances,
        readiness_path: args.readiness.unwrap_or_default(),
        readiness_timeout_ms: args
            .readiness_timeout
            .map(|timeout| timeout.as_millis() as u64),
        stop_signal: args.stop_signal.unwrap_or_default(),
        entrypoint: args.entrypoint.unwrap_or_default(),
        interface: args
            .interface
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        python: args
            .python
            .map(|path| absolute_interpreter(&path, None))
            .unwrap_or_default(),
        processes: args.processes,
        threads: args.threads,
        sync_threads: args.sync_threads,
        concurrency: args.concurrency,
        preload: args
            .preload
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        hooks: args.hooks.unwrap_or_default(),
        factory: args.factory.then_some(true),
        factory_phase: String::new(),
        release: args.release.unwrap_or_default(),
        wait: !args.no_wait,
        hold: args.hold,
        override_freeze: args.override_freeze,
        reflection: None,
        retain_ms: args.retain.map(|retain| retain.as_millis() as u64),
        retain_workers: args.retain_workers,
        max_requests: args.max_requests,
        max_lifetime_ms: args
            .max_lifetime
            .map(|lifetime| lifetime.as_millis() as u64),
        soft_timeout_ms: timeout_ms(&args.timeouts, "soft"),
        accel_redirect: None,
        cgi_local_redirect: None,
        sendfile_roots: args.sendfile_roots,
        sendfile_header: args.sendfile_header,
        hard_timeout_ms: timeout_ms(&args.timeouts, "hard"),
        hard_drain_grace_ms: timeout_ms(&args.timeouts, "hard_drain_grace"),
        shutdown_timeout_ms: timeout_ms(&args.timeouts, "shutdown"),
        loop_stall_timeout_ms: timeout_ms(&args.timeouts, "loop_stall"),
        memory_measure: memory_setting(&args.memory, "measure").unwrap_or_default(),
        memory_soft_limit: memory_setting(&args.memory, "soft_limit")
            .and_then(|text| muru_config::parse_size(&text).ok()),
        memory_soft_for_ms: memory_setting(&args.memory, "soft_for")
            .and_then(|text| muru_config::parse_duration(&text).ok())
            .map(|duration| duration.as_millis() as u64),
        memory_reclaim_first: memory_setting(&args.memory, "reclaim_first")
            .map(|text| text == "true"),
        memory_hard_limit: memory_setting(&args.memory, "hard_limit")
            .and_then(|text| muru_config::parse_size(&text).ok()),
        memory_check_interval_ms: memory_setting(&args.memory, "check_interval")
            .and_then(|text| muru_config::parse_duration(&text).ok())
            .map(|duration| duration.as_millis() as u64),
        env: args
            .env
            .iter()
            .map(|(name, value)| pb::EnvEntry {
                name: name.clone(),
                value: value.clone(),
                ..Default::default()
            })
            .chain(
                args.secrets
                    .iter()
                    .map(|(name, store, key, as_file)| pb::EnvEntry {
                        name: name.clone(),
                        secret_store: store.clone(),
                        secret_key: key.clone(),
                        as_file: *as_file,
                        ..Default::default()
                    }),
            )
            .collect(),
    };
    let release = client.deploy(request).await.map_err(explain)?.into_inner();

    if args.no_wait {
        println!(
            "muru: release {}@{} recorded ({})",
            release.app, release.id, release.state
        );
    } else {
        println!(
            "muru: release {}@{} is {}{}",
            release.app,
            release.id,
            release.state,
            if release.serving {
                " and serving"
            } else if args.hold {
                " and held: not serving"
            } else {
                ""
            }
        );
    }
    Ok(())
}

/// The rollout subcommands (section 9.3).
#[derive(Subcommand)]
pub enum RolloutCommand {
    /// Shift the application's traffic to a release step by step, each
    /// step judged by gates against the release that serves; the last
    /// step passing promotes the release, a failing gate rolls back.
    Canary {
        /// The candidate, as app@release.
        release: String,

        /// The steps, as percentages of the application's requests.
        #[arg(long, value_delimiter = ',', default_value = "10,50")]
        steps: Vec<u32>,

        /// How long each step holds before its gates decide.
        #[arg(long, value_name = "DURATION", default_value = "2m", value_parser = muru_config::parse_duration)]
        hold: Duration,

        /// The 5xx rate the candidate may not exceed, as a percentage.
        #[arg(long = "max-error-rate", value_name = "PERCENT", default_value = "1")]
        max_error_rate: f64,

        /// The 95th percentile request time the candidate may not exceed.
        #[arg(long = "max-p95", value_name = "DURATION", value_parser = muru_config::parse_duration)]
        max_p95: Option<Duration>,

        /// The 99th percentile request time the candidate may not exceed.
        #[arg(long = "max-p99", value_name = "DURATION", value_parser = muru_config::parse_duration)]
        max_p99: Option<Duration>,

        /// Worker crashes and timeout kills the candidate may have in a
        /// step; one more rolls back at once.
        #[arg(long = "max-worker-faults", value_name = "COUNT", default_value = "0")]
        max_worker_faults: u64,

        /// The 95th percentile wait for a worker thread the candidate
        /// may not exceed.
        #[arg(long = "max-queue-wait", value_name = "DURATION", value_parser = muru_config::parse_duration)]
        max_queue_wait: Option<Duration>,

        /// A URL requested through the edge from the candidate and the
        /// baseline every few seconds, by preview; one failing on the
        /// candidate alone rolls back. Repeatable.
        #[arg(long = "probe", value_name = "URL")]
        probes: Vec<String>,

        /// Requests the candidate must serve in a step before the gates
        /// judge it.
        #[arg(long = "min-requests", value_name = "COUNT", default_value = "100")]
        min_requests: u64,

        /// Wait for `rollout resume` before each step after the first.
        #[arg(long)]
        approve_each_step: bool,

        /// Stay until the rollout is promoted or rolled back.
        #[arg(long)]
        wait: bool,

        /// Go ahead although an overlay freezes the application's
        /// deploys (section 9.6); recorded as an event.
        #[arg(long)]
        override_freeze: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Move all of the application's traffic to a release now and drain
    /// the one that served (blue/green).
    Bluegreen {
        /// The release, as app@release.
        release: String,

        /// Go ahead although an overlay freezes the application's
        /// deploys (section 9.6); recorded as an event.
        #[arg(long)]
        override_freeze: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Show the rollouts, of one application or of all.
    Status {
        /// The application.
        app: Option<String>,

        /// Stay until the application's rollout is promoted or rolled
        /// back.
        #[arg(long)]
        wait: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Hold the application's rollout at its current step.
    Pause {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Continue a paused rollout, or approve its next step.
    Resume {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Give the candidate all the traffic now and end the rollout.
    Promote {
        /// The application.
        app: String,

        /// Go ahead although an overlay freezes the application's
        /// deploys (section 9.6); recorded as an event.
        #[arg(long)]
        override_freeze: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Take the candidate out of traffic now and end the rollout.
    Abort {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// What `muructl scale` takes: a group's count to pin or release, or
/// `scale status` for the autoscaler's state (sections 7.3 and 7.4).
#[derive(Args, Clone)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct ScaleArgs {
    #[command(subcommand)]
    pub command: Option<ScaleCommand>,

    /// The release as app@release, or an application for its serving
    /// release.
    #[arg(value_name = "APP[@RELEASE]", required = true)]
    target: Option<String>,

    /// The worker group.
    #[arg(long, default_value = "default", value_name = "NAME")]
    group: String,

    /// Pin the group to this many workers; the autoscaler stands aside
    /// until --unpin.
    #[arg(long, value_name = "COUNT", conflicts_with = "unpin")]
    processes: Option<u32>,

    /// Let the autoscaler decide the group's count again.
    #[arg(long)]
    unpin: bool,

    /// Keep at least this many workers until --until or for --for, a
    /// temporary schedule over the application (section 7.5).
    #[arg(long = "min", value_name = "COUNT", conflicts_with_all = ["processes", "unpin"])]
    min_workers: Option<u32>,

    /// With --min, allow up to this many workers meanwhile.
    #[arg(long = "max", value_name = "COUNT", requires = "min_workers")]
    max_workers: Option<u32>,

    /// When the floor ends, such as 2026-11-30T23:59 or an RFC 3339 time.
    #[arg(
        long,
        value_name = "TIME",
        requires = "min_workers",
        conflicts_with = "duration"
    )]
    until: Option<String>,

    /// How long the floor lasts, such as 4h.
    #[arg(long = "for", value_name = "DURATION", value_parser = muru_config::parse_duration, requires = "min_workers")]
    duration: Option<Duration>,

    /// The time zone --until is in; the local zone without it.
    #[arg(long, value_name = "ZONE", requires = "until")]
    timezone: Option<String>,

    #[command(flatten)]
    socket: SocketArgs,
}

/// The `muructl scale` subcommands.
#[derive(Subcommand, Clone)]
pub enum ScaleCommand {
    /// Show each worker group's policy, workers, signals, last decision
    /// and churn.
    Status {
        /// The release as app@release, or an application for its serving
        /// release.
        #[arg(value_name = "APP[@RELEASE]")]
        target: String,

        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// Pin or release a group's count, or show the autoscaler's state.
pub async fn scale(args: ScaleArgs) -> anyhow::Result<()> {
    if let Some(ScaleCommand::Status { target, socket }) = args.command {
        let (app, release) = split_target(&target)?;
        let mut client = client::connect(&socket.socket()).await?;
        let status = client
            .get_scale_status(pb::GetScaleStatusRequest {
                app: app.to_owned(),
                release: release.to_owned(),
            })
            .await
            .map_err(explain)?
            .into_inner();
        print_scale_status(&status);
        return Ok(());
    }

    let target = args.target.unwrap_or_default();
    let (app, release) = split_target(&target)?;

    if args.processes.is_none() && !args.unpin && args.min_workers.is_none() {
        anyhow::bail!(
            "give --processes COUNT to pin the group, --unpin, or --min COUNT with --until or --for; `scale status` shows the state"
        );
    }

    let until_unix_ms = match (&args.until, args.duration) {
        (Some(text), _) => muru_config::parse_moment(text, args.timezone.as_deref())
            .map_err(|error| anyhow::anyhow!("--until: {error}"))?
            .max(0) as u64,
        (None, Some(duration)) => now_unix_ms() + duration.as_millis() as u64,
        (None, None) if args.min_workers.is_some() => {
            anyhow::bail!("--min needs --until TIME or --for DURATION")
        }
        (None, None) => 0,
    };

    let mut client = client::connect(&args.socket.socket()).await?;
    let status = client
        .set_scale(pb::SetScaleRequest {
            app: app.to_owned(),
            release: release.to_owned(),
            group: args.group.clone(),
            processes: args.processes,
            unpin: args.unpin,
            min_workers: args.min_workers,
            max_workers: args.max_workers,
            until_unix_ms,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if let Some(floor) = args.min_workers {
        println!(
            "muru: {app} {} kept at {floor} workers or more until {}",
            args.group,
            format_time(until_unix_ms)
        );
        return Ok(());
    }

    match args.processes {
        Some(count) => println!(
            "muru: {}@{} {} pinned to {count} workers",
            status.app, status.release, args.group
        ),
        None => println!(
            "muru: {}@{} {} unpinned; the autoscaler decides again",
            status.app, status.release, args.group
        ),
    }

    Ok(())
}

/// An application, or app@release; an empty release means the serving one.
fn split_target(text: &str) -> anyhow::Result<(&str, &str)> {
    match text.split_once('@') {
        Some((app, release)) if !app.is_empty() && !release.is_empty() => Ok((app, release)),
        Some(_) => anyhow::bail!("{text:?} is not a release: expected app@release"),
        None if text.is_empty() => anyhow::bail!("name an application or a release"),
        None => Ok((text, "")),
    }
}

/// The autoscaler's state, a row per group with its last decision below.
fn print_scale_status(status: &pb::ScaleStatus) {
    println!("{}@{}", status.app, status.release);
    println!(
        "{:<12} {:<12} {:<10} {:<7} {:<10} {:<10} POLICY",
        "GROUP", "WORKERS", "SLOTS", "IDLE", "QUEUE P95", "REAPS"
    );

    for group in &status.groups {
        let workers = if group.pinned {
            format!("{} pinned", group.desired)
        } else if group.desired != group.ready {
            format!("{} -> {}", group.ready, group.desired)
        } else {
            group.ready.to_string()
        };

        let reaps = if group.mode == "static" {
            "-".to_owned()
        } else if group.suspended {
            format!("{}/{} halt", group.reaps_last_hour, group.churn_budget)
        } else {
            format!("{}/{}", group.reaps_last_hour, group.churn_budget)
        };

        let p95 = match group.queue_wait_p95_ms {
            Some(ms) => format!("{ms:.0} ms"),
            None => "-".to_owned(),
        };

        println!(
            "{:<12} {:<12} {:<10} {:<7} {:<10} {:<10} {}",
            group.group,
            workers,
            format!("{:.0}/{:.0}", group.slots_busy, group.slots_total),
            format!("{:.1}", group.idle_workers),
            p95,
            reaps,
            group.policy
        );

        if !group.capacity_from.is_empty() {
            println!(
                "  scheduled: {}-{} workers{} from {}",
                group.floor,
                group.ceiling,
                if group.scale_in_held {
                    ", no scale-in"
                } else {
                    ""
                },
                group.capacity_from.join(", ")
            );
        }

        // Section 7.3: the workers' own signals, and what they say.

        let measured = |value: Option<f64>, unit: &str| {
            value.map_or("-".to_owned(), |value| format!("{value:.0}{unit}"))
        };
        println!(
            "  workers: cpu {}, gil wait p95 {}, loop lag p95 {}",
            measured(group.worker_cpu.map(|cpu| cpu * 100.0), "%"),
            measured(group.gil_wait_p95_ms, " ms"),
            measured(group.loop_lag_p95_ms, " ms")
        );

        if !group.diagnosis.is_empty() {
            if group.advice.is_empty() {
                println!("  diagnosis: {}", group.diagnosis);
            } else {
                println!(
                    "  diagnosis: {}; for the next release, {}",
                    group.diagnosis, group.advice
                );
            }
        }

        if !group.last_decision.is_empty() {
            println!(
                "  last: {} ({} ago)",
                group.last_decision,
                muru_config::format_duration(Duration::from_secs(
                    group.last_decision_age_ms / 1000
                ))
            );
        }
    }
}

/// Run a rollout subcommand.
pub async fn rollout(command: RolloutCommand) -> anyhow::Result<()> {
    match command {
        RolloutCommand::Canary {
            release,
            steps,
            hold,
            max_error_rate,
            max_p95,
            max_p99,
            max_worker_faults,
            max_queue_wait,
            probes,
            min_requests,
            approve_each_step,
            wait,
            override_freeze,
            socket,
        } => {
            let (app, id) = split_release(&release)?;
            let mut client = client::connect(&socket.socket()).await?;
            let rollout = client
                .start_rollout(pb::StartRolloutRequest {
                    app: app.to_owned(),
                    release: id.to_owned(),
                    steps: steps
                        .iter()
                        .map(|percent| pb::RolloutStep {
                            percent: *percent,
                            hold_ms: hold.as_millis() as u64,
                        })
                        .collect(),
                    max_error_rate: max_error_rate / 100.0,
                    max_p95_ms: max_p95.map_or(0, |p95| p95.as_millis() as u64),
                    max_p99_ms: max_p99.map_or(0, |p99| p99.as_millis() as u64),
                    max_worker_faults,
                    max_queue_wait_p95_ms: max_queue_wait.map_or(0, |wait| wait.as_millis() as u64),
                    probes,
                    min_requests,
                    approve_each_step,
                    override_freeze,
                })
                .await
                .map_err(explain)?
                .into_inner();
            println!(
                "muru: rollout {} of {}@{} started against {}: {}",
                rollout.id, rollout.app, rollout.candidate, rollout.baseline, rollout.message
            );

            if wait {
                let finished = wait_rollout(&mut client, app, &rollout.id).await?;
                println!(
                    "muru: rollout {} {}: {}",
                    finished.id, finished.state, finished.message
                );
            }

            Ok(())
        }
        RolloutCommand::Bluegreen {
            release,
            override_freeze,
            socket,
        } => {
            let (app, id) = split_release(&release)?;
            let mut client = client::connect(&socket.socket()).await?;
            let release = client
                .promote_release(pb::PromoteReleaseRequest {
                    app: app.to_owned(),
                    release: id.to_owned(),
                    override_freeze,
                })
                .await
                .map_err(explain)?
                .into_inner();
            println!(
                "muru: release {}@{} is {} and serving",
                release.app, release.id, release.state
            );
            Ok(())
        }
        RolloutCommand::Status { app, wait, socket } => {
            let mut client = client::connect(&socket.socket()).await?;

            if wait {
                let Some(app) = app else {
                    anyhow::bail!("--wait needs an application");
                };
                let finished = wait_rollout(&mut client, &app, "").await?;
                println!(
                    "muru: rollout {} {}: {}",
                    finished.id, finished.state, finished.message
                );
                return Ok(());
            }

            let rollouts = client
                .list_rollouts(pb::ListRolloutsRequest {
                    app: app.unwrap_or_default(),
                })
                .await
                .map_err(explain)?
                .into_inner()
                .rollouts;

            if rollouts.is_empty() {
                println!("no rollouts");
                return Ok(());
            }

            println!(
                "{:<16} {:<8} {:<12} {:<12} {:<18} {:<8} MESSAGE",
                "APP", "ROLLOUT", "CANDIDATE", "BASELINE", "STATE", "TRAFFIC"
            );

            for rollout in rollouts {
                println!(
                    "{:<16} {:<8} {:<12} {:<12} {:<18} {:<8} {}",
                    rollout.app,
                    rollout.id,
                    rollout.candidate,
                    rollout.baseline,
                    rollout.state,
                    format!("{}%", rollout.percent),
                    rollout.message
                );
            }

            Ok(())
        }
        RolloutCommand::Pause { app, socket } => {
            let mut client = client::connect(&socket.socket()).await?;
            let rollout = client
                .pause_rollout(pb::RolloutControlRequest {
                    app,
                    override_freeze: false,
                })
                .await
                .map_err(explain)?
                .into_inner();
            println!(
                "muru: rollout {} {}: {}",
                rollout.id, rollout.state, rollout.message
            );
            Ok(())
        }
        RolloutCommand::Resume { app, socket } => {
            let mut client = client::connect(&socket.socket()).await?;
            let rollout = client
                .resume_rollout(pb::RolloutControlRequest {
                    app,
                    override_freeze: false,
                })
                .await
                .map_err(explain)?
                .into_inner();
            println!(
                "muru: rollout {} {}: {}",
                rollout.id, rollout.state, rollout.message
            );
            Ok(())
        }
        RolloutCommand::Promote {
            app,
            override_freeze,
            socket,
        } => {
            let mut client = client::connect(&socket.socket()).await?;
            let rollout = client
                .promote_rollout(pb::RolloutControlRequest {
                    app,
                    override_freeze,
                })
                .await
                .map_err(explain)?
                .into_inner();
            println!(
                "muru: rollout {} {}: {}",
                rollout.id, rollout.state, rollout.message
            );
            Ok(())
        }
        RolloutCommand::Abort { app, socket } => {
            let mut client = client::connect(&socket.socket()).await?;
            let rollout = client
                .abort_rollout(pb::RolloutControlRequest {
                    app,
                    override_freeze: false,
                })
                .await
                .map_err(explain)?
                .into_inner();
            println!(
                "muru: rollout {} {}: {}",
                rollout.id, rollout.state, rollout.message
            );
            Ok(())
        }
    }
}

/// Poll a rollout until it is promoted or rolled back, printing each
/// change of step or state on the way.
async fn wait_rollout(
    client: &mut pb::admin_client::AdminClient<tonic::transport::Channel>,
    app: &str,
    id: &str,
) -> anyhow::Result<pb::Rollout> {
    let mut last = String::new();

    loop {
        let rollout = client
            .get_rollout(pb::GetRolloutRequest {
                app: app.to_owned(),
                rollout: id.to_owned(),
            })
            .await
            .map_err(explain)?
            .into_inner();

        if matches!(rollout.state.as_str(), "promoted" | "rolled_back") {
            return Ok(rollout);
        }

        let line = format!("{} {}", rollout.state, rollout.message);

        if line != last {
            println!("muru: rollout {} {line}", rollout.id);
            last = line;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The application subcommands.
#[derive(Subcommand)]
pub enum AppCommand {
    /// List the applications.
    List {
        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// List applications.
pub async fn app_list(socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let apps = client
        .list_apps(pb::ListAppsRequest {})
        .await?
        .into_inner()
        .apps;

    if apps.is_empty() {
        println!("no applications");
        return Ok(());
    }

    println!("{:<20} {:<12} {:<12} USER", "APP", "CURRENT", "PREVIOUS");

    for app in apps {
        let user = if app.user.is_empty() {
            "-".to_owned()
        } else if app.group.is_empty() {
            app.user.clone()
        } else {
            format!("{}:{}", app.user, app.group)
        };
        println!(
            "{:<20} {:<12} {:<12} {user}{}{}{}",
            app.name,
            or_dash(&app.current_release),
            or_dash(&app.previous_release),
            if app.umask.is_empty() {
                String::new()
            } else {
                format!(" umask {}", app.umask)
            },
            if app.priority == 0 {
                String::new()
            } else {
                format!(" [priority {}]", app.priority)
            },
            if app.maintenance {
                " [maintenance]"
            } else {
                ""
            }
        );
    }

    Ok(())
}

/// The `muructl overlay` subcommands (section 9.10).
#[derive(Subcommand)]
pub enum OverlayCommand {
    /// Every overlay, whether it is active and what it changes.
    List {
        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Apply an overlay now, until ended, or for a time.
    Activate {
        /// The overlay.
        name: String,

        /// End it on its own after this long, for example 2h.
        #[arg(long = "for", value_name = "DURATION", value_parser = muru_config::parse_duration, conflicts_with = "until")]
        duration: Option<Duration>,

        /// End it on its own at this time, such as 2026-11-02T09:00 or an
        /// RFC 3339 time.
        #[arg(long, value_name = "TIME")]
        until: Option<String>,

        /// The time zone --until is in; the local zone without it.
        #[arg(long, value_name = "ZONE", requires = "until")]
        timezone: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Stop applying an overlay; one its schedule activated stays ended
    /// until the schedule's window is over.
    End {
        /// The overlay.
        name: String,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// What activating an overlay now would change.
    Preview {
        /// The overlay.
        name: String,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Fold an active overlay's changes into the base configuration and
    /// end it for good.
    Promote {
        /// The overlay.
        name: String,

        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// List, activate, end, preview or promote overlays.
pub async fn overlay(command: OverlayCommand) -> anyhow::Result<()> {
    let (name, action, until, socket) = match command {
        OverlayCommand::List { socket } => return list_overlays(&socket).await,
        OverlayCommand::Preview { name, socket } => {
            let mut client = client::connect(&socket.socket()).await?;
            let preview = client
                .preview_overlay(pb::PreviewOverlayRequest { name: name.clone() })
                .await
                .map_err(explain)?
                .into_inner();

            if preview.changes.is_empty() {
                println!("muru: activating overlay {name} would change nothing now");
            }

            for change in preview.changes {
                println!("{change}");
            }

            return Ok(());
        }
        OverlayCommand::Activate {
            name,
            duration,
            until,
            timezone,
            socket,
        } => {
            let until = match (duration, until) {
                (Some(duration), _) => Some(now_unix_ms() + duration.as_millis() as u64),
                (None, Some(text)) => Some(
                    muru_config::parse_moment(&text, timezone.as_deref())
                        .map_err(anyhow::Error::msg)?
                        .max(0) as u64,
                ),
                (None, None) => None,
            };

            (name, "activate", until, socket)
        }
        OverlayCommand::End { name, socket } => (name, "end", None, socket),
        OverlayCommand::Promote { name, socket } => (name, "promote", None, socket),
    };

    let mut client = client::connect(&socket.socket()).await?;
    let response = client
        .control_overlay(pb::ControlOverlayRequest {
            name: name.clone(),
            action: action.to_owned(),
            until_unix_ms: until.unwrap_or(0),
        })
        .await
        .map_err(explain)?
        .into_inner();
    println!(
        "muru: overlay {name} {}{}",
        response.message,
        until
            .map(|until| format!(", until {}", format_time(until)))
            .unwrap_or_default()
    );
    Ok(())
}

async fn list_overlays(socket: &SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let overlays = client
        .list_overlays(pb::ListOverlaysRequest {})
        .await
        .map_err(explain)?
        .into_inner()
        .overlays;

    if overlays.is_empty() {
        println!("muru: no overlays are declared");
        return Ok(());
    }

    println!(
        "{:<24} {:<10} {:<34} CHANGES",
        "OVERLAY", "STATE", "TRIGGER"
    );

    for overlay in overlays {
        let mut state = if overlay.state.is_empty() {
            "idle".to_owned()
        } else if overlay.active {
            format!("active ({})", overlay.state)
        } else {
            overlay.state.clone()
        };

        if overlay.until_unix_ms > 0 {
            state.push_str(&format!(" until {}", format_time(overlay.until_unix_ms)));
        } else if overlay.starts_unix_ms > 0 {
            state.push_str(&format!(" from {}", format_time(overlay.starts_unix_ms)));
        }

        println!(
            "{:<24} {:<10} {:<34} {}",
            overlay.name,
            state,
            overlay.trigger,
            overlay.changes.join(", ")
        );
    }

    Ok(())
}

/// The `muructl maintenance` subcommands (section 4.12).
#[derive(Subcommand)]
pub enum MaintenanceCommand {
    /// Serve the application's maintenance page at the edge: 503
    /// maintenance, except for what its exemptions let through.
    On {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Serve the application as usual again.
    Off {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// Turn maintenance mode on or off.
pub async fn maintenance(command: MaintenanceCommand) -> anyhow::Result<()> {
    let (app, on, socket) = match command {
        MaintenanceCommand::On { app, socket } => (app, true, socket),
        MaintenanceCommand::Off { app, socket } => (app, false, socket),
    };

    let mut client = client::connect(&socket.socket()).await?;
    let status = client
        .set_maintenance(pb::SetMaintenanceRequest { app, on })
        .await
        .map_err(explain)?
        .into_inner();

    if status.on {
        println!(
            "muru: {} is in maintenance: clients get 503 maintenance until `muructl maintenance off {}`",
            status.app, status.app
        );
    } else {
        println!("muru: {} serves as usual again", status.app);
    }

    Ok(())
}

/// The dashboard subcommands (section 10.7).
#[derive(Subcommand)]
pub enum DashboardCommand {
    /// Print a single-use link that signs a browser in to the dashboard's
    /// actions, which the [dashboard] table's `actions = true` enables.
    Login {
        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// Run a dashboard subcommand.
pub async fn dashboard(command: DashboardCommand) -> anyhow::Result<()> {
    let DashboardCommand::Login { socket } = command;
    let mut client = client::connect(&socket.socket()).await?;
    let link = client
        .dashboard_login(pb::DashboardLoginRequest {})
        .await
        .map_err(explain)?
        .into_inner();
    println!("{}", link.url);
    eprintln!(
        "muru: open the link within {} to sign in; it works once",
        muru_config::format_duration(Duration::from_secs(link.expires_in_s))
    );
    Ok(())
}

/// The virtual host subcommands.
#[derive(Subcommand)]
pub enum HostCommand {
    /// List the virtual hosts.
    List {
        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// List virtual hosts.
pub async fn host_list(socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let hosts = client
        .list_hosts(pb::ListHostsRequest {})
        .await?
        .into_inner()
        .hosts;

    if hosts.is_empty() {
        println!("no virtual hosts");
        return Ok(());
    }

    println!("{:<16} {:<8} {:<10} NAMES", "HOST", "TLS", "LISTENERS");

    for host in hosts {
        let mut names = host.names.join(", ");

        if host.default {
            names.push_str(if names.is_empty() {
                "(default)"
            } else {
                " (default)"
            });
        }

        println!(
            "{:<16} {:<8} {:<10} {names}",
            host.name,
            if host.tls { "yes" } else { "" },
            if host.listeners.is_empty() {
                "all".to_owned()
            } else {
                host.listeners.join(",")
            }
        );
    }

    Ok(())
}

/// The upstream subcommands (section 4.13).
#[derive(Subcommand)]
pub enum UpstreamCommand {
    /// List the HTTP upstreams.
    List {
        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// List HTTP upstreams.
pub async fn upstream_list(socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let upstreams = client
        .list_upstreams(pb::ListUpstreamsRequest {})
        .await?
        .into_inner()
        .upstreams;

    if upstreams.is_empty() {
        println!("no upstreams");
        return Ok(());
    }

    println!(
        "{:<16} {:<8} {:<6} ADDRESSES",
        "UPSTREAM", "PROTOCOL", "TLS"
    );

    for upstream in upstreams {
        let mut addresses = upstream.addresses.join(", ");

        if !upstream.health_path.is_empty() {
            addresses.push_str(&format!(" [probing {}]", upstream.health_path));
        }

        if upstream.trust_muru_headers {
            addresses.push_str(" [trusts X-Muru headers]");
        }

        println!(
            "{:<16} {:<8} {:<6} {addresses}",
            upstream.name,
            upstream.protocol,
            if upstream.tls { "yes" } else { "" }
        );
    }

    Ok(())
}

/// The route subcommands.
#[derive(Subcommand)]
pub enum RouteCommand {
    /// List the routes.
    List {
        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// List routes.
pub async fn route_list(socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let routes = client
        .list_routes(pb::ListRoutesRequest {})
        .await?
        .into_inner()
        .routes;

    if routes.is_empty() {
        println!("no routes");
        return Ok(());
    }

    println!("{:<16} {:<24} TARGET", "HOST", "PATH");

    for route in routes {
        // Section 4.13: an upstream as the target, or behind the files.

        let mut target = if !route.upstream.is_empty() {
            format!("upstream {}", route.upstream)
        } else if route.static_dir.is_empty() {
            route.app
        } else if !route.fallback_upstream.is_empty() {
            format!(
                "static {}, else upstream {}",
                route.static_dir, route.fallback_upstream
            )
        } else if route.fallback_app.is_empty() {
            format!("static {}", route.static_dir)
        } else {
            format!("static {}, else {}", route.static_dir, route.fallback_app)
        };

        // Section 9.4: the policy, when the route has one, with the
        // releases it gives client classes (section 4.14).

        if !route.traffic.is_empty()
            || route.sticky
            || !route.hash_key.is_empty()
            || !route.traffic_classes.is_empty()
        {
            let mut parts: Vec<String> = route
                .traffic
                .iter()
                .map(|share| format!("{} {}%", share.release, share.percent))
                .collect();
            let mut classes: Vec<_> = route.traffic_classes.iter().collect();
            classes.sort();
            parts.extend(
                classes
                    .into_iter()
                    .map(|(class, release)| format!("{class} to {release}")),
            );

            if route.sticky {
                parts.push("sticky".to_owned());
            }

            if !route.hash_key.is_empty() {
                parts.push(format!("by {}", route.hash_key));
            }

            target = format!("{target} ({})", parts.join(", "));
        }

        // Section 4.10: reachable by internal redirect only, and from
        // which other applications.

        if route.internal {
            target.push_str(" [internal]");
        }

        if !route.accept_redirects_from.is_empty() {
            target.push_str(&format!(
                " [redirects from {}]",
                route.accept_redirects_from.join(", ")
            ));
        }

        // Section 4.16: the handler asked before the route is served.

        if !route.auth.is_empty() {
            target.push_str(&format!(" [auth by {}]", route.auth));
        }

        // Sections 4.6 and 4.11: the route's body limit and uploads.

        if !route.max_body_size.is_empty() {
            target.push_str(&format!(" [max body {}]", route.max_body_size));
        }

        if !route.upload.is_empty() {
            target.push_str(&format!(" [uploads {}]", route.upload));
        }

        // Section 4.14: the client classes the route is for.

        if !route.client_classes.is_empty() {
            target.push_str(&format!(" [for {}]", route.client_classes.join(", ")));
        }

        println!("{:<16} {:<24} {target}", route.host, route.path);
    }

    Ok(())
}

/// The release subcommands.
#[derive(Subcommand)]
pub enum ReleaseCommand {
    /// List releases, of one application or of all.
    List {
        /// The application.
        app: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Wait until a release reaches a state.
    Wait {
        /// The release, as app@release.
        release: String,

        /// The states to wait for (default: ready or failed).
        #[arg(long = "state", value_name = "STATE")]
        states: Vec<String>,

        /// How long to wait, in seconds (default 300).
        #[arg(long, default_value = "300")]
        timeout: u64,

        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// List releases.
pub async fn release_list(app: Option<String>, socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let releases = client
        .list_releases(pb::ListReleasesRequest {
            app: app.unwrap_or_default(),
        })
        .await?
        .into_inner()
        .releases;

    if releases.is_empty() {
        println!("no releases");
        return Ok(());
    }

    println!(
        "{:<24} {:<10} {:<8} {:<10} SOURCE",
        "RELEASE", "STATE", "SERVING", "WORKERS"
    );

    for release in releases {
        println!(
            "{:<24} {:<10} {:<8} {:<10} {}",
            format!("{}@{}", release.app, release.id),
            release.state,
            if release.serving { "yes" } else { "" },
            if release.kind == "http" {
                format!("{} {}", release.instances, release.profile)
            } else {
                format!("{}x{}", release.processes, release.threads)
            },
            release.source
        );

        if !release.message.is_empty() {
            println!("{:<24} {}", "", release.message);
        }
    }

    Ok(())
}

/// Wait for a release to reach a state.
pub async fn release_wait(
    release: String,
    states: Vec<String>,
    timeout: u64,
    socket: SocketArgs,
) -> anyhow::Result<()> {
    let (app, id) = split_release(&release)?;
    let mut client = client::connect(&socket.socket()).await?;
    let release = client
        .wait_release(pb::WaitReleaseRequest {
            app: app.to_owned(),
            release: id.to_owned(),
            states,
            timeout_ms: timeout * 1000,
        })
        .await
        .map_err(explain)?
        .into_inner();

    println!(
        "muru: release {}@{} is {}",
        release.app, release.id, release.state
    );

    if !release.message.is_empty() {
        println!("muru: {}", release.message);
    }

    Ok(())
}

/// Roll an application back to its previous release.
pub async fn rollback(app: String, socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let release = client
        .rollback(pb::RollbackRequest { app })
        .await
        .map_err(explain)?
        .into_inner();

    println!(
        "muru: rolled back to {}@{} ({})",
        release.app, release.id, release.state
    );
    Ok(())
}

/// Drain a release.
pub async fn drain(release: String, socket: SocketArgs) -> anyhow::Result<()> {
    let (app, id) = split_release(&release)?;
    let mut client = client::connect(&socket.socket()).await?;
    let release = client
        .drain_release(pb::DrainReleaseRequest {
            app: app.to_owned(),
            release: id.to_owned(),
        })
        .await
        .map_err(explain)?
        .into_inner();

    println!(
        "muru: release {}@{} is {}",
        release.app, release.id, release.state
    );
    Ok(())
}

/// What `apply` takes.
#[derive(Args)]
pub struct ApplyArgs {
    /// The file to apply, or a directory whose .toml files are applied together.
    #[arg(short = 'f', long = "file", value_name = "FILE or DIR")]
    file: PathBuf,

    /// Report what would change and change nothing.
    #[arg(long)]
    dry_run: bool,

    /// Also remove the applications and releases the files do not contain.
    #[arg(long)]
    prune: bool,

    /// Deploy releases of applications an overlay freezes (section 9.6);
    /// recorded as an event.
    #[arg(long)]
    override_freeze: bool,

    /// Print the report as JSON.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    socket: SocketArgs,
}

/// Apply declarative configuration. The exit code is 0 when nothing
/// changed or everything applied, 2 for a dry run with changes planned
/// and 1 for errors.
pub async fn apply(args: ApplyArgs) -> anyhow::Result<i32> {
    let document = load_documents(&args.file)?;
    let mut client = client::connect(&args.socket.socket()).await?;
    let report = client
        .apply_config(pb::ApplyConfigRequest {
            document: document.to_toml().context("writing the configuration")?,
            dry_run: args.dry_run,
            prune: args.prune,
            override_freeze: args.override_freeze,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report_json(&report))?);
    } else {
        print_report(&report, args.dry_run);
    }
    Ok(exit_code(&report, args.dry_run))
}

/// The configuration subcommands.
#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Write desired state as a file that applies back without changes.
    Export {
        /// One application rather than all.
        #[arg(long, value_name = "NAME")]
        app: Option<String>,

        /// Every release of each application, not only the one that serves.
        #[arg(long)]
        all_releases: bool,

        /// Write each rollout under way as where it stands: its candidate
        /// declared, and the application's routes splitting traffic at
        /// its current share.
        #[arg(long)]
        resolve_rollouts: bool,

        /// Write JSON rather than TOML.
        #[arg(long)]
        json: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Show desired state; with --effective, as the active overlays leave
    /// it, with what each sets that is in force.
    Show {
        /// One application rather than all.
        app: Option<String>,

        /// Apply the active overlays and attribute what they set.
        #[arg(long)]
        effective: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Compare a file, or a directory of files, with desired state.
    Diff {
        /// The file, or a directory whose .toml files are read together.
        #[arg(short = 'f', long = "file", value_name = "FILE or DIR")]
        file: PathBuf,

        /// Print the report as JSON.
        #[arg(long)]
        json: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// Export desired state, with a header naming where it came from.
pub async fn config_export(
    app: Option<String>,
    all_releases: bool,
    resolve_rollouts: bool,
    json: bool,
    socket: SocketArgs,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let export = client
        .export_config(pb::ExportConfigRequest {
            app: app.unwrap_or_default(),
            all_releases,
            effective: false,
            resolve_rollouts,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        let document =
            muru_config::Document::parse(&export.document).map_err(anyhow::Error::msg)?;
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }

    println!(
        "# Muru desired state: muructl {} on {}, {}, state version {}.\n\
         # Applying this file to the daemon it came from changes nothing.\n",
        export.version,
        export.node,
        format_time(export.time_unix_ms),
        export.state_version
    );
    print!("{}", export.document);
    Ok(())
}

/// Show desired state, or with `effective` the state the active overlays
/// make of it, headed by what each sets that is in force.
pub async fn config_show(
    app: Option<String>,
    effective: bool,
    socket: SocketArgs,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let export = client
        .export_config(pb::ExportConfigRequest {
            app: app.unwrap_or_default(),
            all_releases: false,
            effective,
            resolve_rollouts: false,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if !effective {
        print!("{}", export.document);
        return Ok(());
    }

    println!(
        "# Muru effective state: muructl {} on {}, {}, state version {}.",
        export.version,
        export.node,
        format_time(export.time_unix_ms),
        export.state_version
    );

    if export.attributions.is_empty() {
        println!("# No active overlay changes this configuration.\n");
    } else {
        println!("# The active overlays set, and the routes below include:");

        for line in &export.attributions {
            println!("#   {line}");
        }

        println!();
    }

    if !export.layers.is_empty() {
        println!("# The releases' settings, and the layer each came from:");

        for line in &export.layers {
            println!("#   {line}");
        }

        println!();
    }

    print!("{}", export.document);
    Ok(())
}

/// Compare a file with desired state. The exit code is 0 when they
/// match, 2 when they differ and 1 for errors.
pub async fn config_diff(file: PathBuf, json: bool, socket: SocketArgs) -> anyhow::Result<i32> {
    let document = load_documents(&file)?;
    let mut client = client::connect(&socket.socket()).await?;
    let report = client
        .diff_config(pb::DiffConfigRequest {
            document: document.to_toml().context("writing the configuration")?,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        println!("{}", serde_json::to_string_pretty(&report_json(&report))?);
    } else {
        print_report(&report, true);
    }
    Ok(exit_code(&report, true))
}

/// Read one file, or every .toml file of a directory in name order, into
/// one document, with relative source paths taken from each file's
/// directory.
fn load_documents(path: &Path) -> anyhow::Result<muru_config::Document> {
    let mut files = Vec::new();

    if path.is_dir() {
        for entry in
            std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))?
        {
            let entry = entry?.path();
            if entry
                .extension()
                .is_some_and(|extension| extension == "toml")
            {
                files.push(entry);
            }
        }
        files.sort();

        if files.is_empty() {
            anyhow::bail!("{} holds no .toml files", path.display());
        }
    } else {
        files.push(path.to_path_buf());
    }

    let mut document = muru_config::Document::default();

    for file in files {
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let mut parsed = muru_config::Document::parse(&text)
            .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;

        // Sources are resolved here, where the file is, since the daemon
        // may run elsewhere.

        let base = file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let absolute = |path: &str| {
            if Path::new(path).is_relative() {
                base.join(path).display().to_string()
            } else {
                path.to_owned()
            }
        };

        for app in parsed.app.values_mut() {
            for release in app.release.values_mut() {
                if let Some(source) = &release.source {
                    release.source = Some(absolute(source));
                }

                if let Some(python) = &release.python {
                    release.python = Some(absolute_interpreter(Path::new(python), Some(&base)));
                }
            }
        }

        for host in parsed.host.values_mut() {
            if let Some(tls) = &mut host.tls {
                tls.certificate = absolute(&tls.certificate);
                tls.key = absolute(&tls.key);
            }
        }

        document
            .merge(parsed)
            .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;
    }

    Ok(document)
}

/// Print a report as people read it.
fn print_report(report: &pb::ConfigReport, dry_run: bool) {
    if !report.errors.is_empty() {
        println!("muru: nothing applied; the configuration has errors:");

        for error in &report.errors {
            println!("  {error}");
        }
    } else if report.changes.is_empty() {
        println!(
            "muru: no changes ({} object{} as declared)",
            report.unchanged,
            if report.unchanged == 1 { "" } else { "s" }
        );
    } else {
        println!(
            "muru: {}",
            if report.applied {
                "applied"
            } else if dry_run {
                "plan (nothing changed)"
            } else {
                "plan"
            }
        );

        for change in &report.changes {
            let what = if report.applied {
                change.outcome.as_str()
            } else {
                change.detail.as_str()
            };
            println!("  {:<15} {:<24} {what}", change.action, change.subject);
        }

        if report.unchanged > 0 {
            println!(
                "  {} object{} as declared",
                report.unchanged,
                if report.unchanged == 1 { "" } else { "s" }
            );
        }
    }

    for warning in &report.warnings {
        println!("muru: warning: {warning}");
    }
}

/// Section 10.4: 0 for no changes, 2 for changes planned, 1 for errors;
/// after an apply, 1 when any change failed.
fn exit_code(report: &pb::ConfigReport, planning: bool) -> i32 {
    if !report.errors.is_empty() {
        return 1;
    }

    if report.applied {
        let failed = report
            .changes
            .iter()
            .any(|change| change.outcome.starts_with("failed"));
        return if failed { 1 } else { 0 };
    }

    if planning && !report.changes.is_empty() {
        return 2;
    }

    0
}

fn report_json(report: &pb::ConfigReport) -> serde_json::Value {
    serde_json::json!({
        "errors": report.errors,
        "warnings": report.warnings,
        "changes": report.changes.iter().map(|change| serde_json::json!({
            "action": change.action,
            "subject": change.subject,
            "detail": change.detail,
            "outcome": change.outcome,
        })).collect::<Vec<_>>(),
        "unchanged": report.unchanged,
        "applied": report.applied,
    })
}

/// The time now, as Unix milliseconds.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Unix milliseconds as an RFC 3339 instant in UTC.
fn format_time(unix_ms: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp((unix_ms / 1000) as i64)
        .map(|time| {
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                time.year(),
                time.month() as u8,
                time.day(),
                time.hour(),
                time.minute(),
                time.second()
            )
        })
        .unwrap_or_default()
}

/// Show or follow the events.
pub async fn events(follow: bool, after: u64, socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;

    if !follow {
        let events = client
            .list_events(pb::ListEventsRequest { after, limit: 200 })
            .await?
            .into_inner()
            .events;

        for event in events {
            println!("{}", format_event(&event));
        }

        return Ok(());
    }

    let mut stream = client
        .watch_events(pb::WatchEventsRequest { after })
        .await?
        .into_inner();

    while let Some(event) = stream.next().await {
        println!("{}", format_event(&event?));
    }

    Ok(())
}

/// What `muructl logs` takes: a release's output, or the access log,
/// shown or followed; or `logs reopen` after an external rotation.
#[derive(Args, Clone)]
#[command(args_conflicts_with_subcommands = true)]
pub struct LogsArgs {
    #[command(subcommand)]
    pub command: Option<LogsCommand>,

    /// The application, or app@release; the current release when only
    /// the application is named.
    #[arg(value_name = "APP[@RELEASE]", required_unless_present = "access")]
    target: Option<String>,

    /// Keep printing lines as they are written.
    #[arg(long, short)]
    follow: bool,

    /// How many recent lines to show first.
    #[arg(long, short = 'n', default_value = "100", value_name = "COUNT")]
    lines: u32,

    /// The access log the edge writes, instead of a release's output.
    #[arg(long)]
    access: bool,

    /// Print access log records as the JSON lines they are kept as.
    #[arg(long, requires = "access")]
    json: bool,

    #[command(flatten)]
    socket: SocketArgs,
}

/// The `muructl logs` subcommands.
#[derive(Subcommand, Clone)]
pub enum LogsCommand {
    /// Reopen every log file by name, after logrotate has renamed them.
    Reopen {
        /// Also run on_reopen_logs in every worker, for files the
        /// application opened itself.
        #[arg(long)]
        workers: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// Show or follow a release's output or the access log.
pub async fn logs(args: LogsArgs) -> anyhow::Result<()> {
    let (app, release) = match &args.target {
        Some(target) if target.contains('@') => {
            let (app, release) = split_release(target)?;
            (app.to_owned(), release.to_owned())
        }
        Some(app) => (app.clone(), String::new()),
        None => (String::new(), String::new()),
    };

    let mut client = client::connect(&args.socket.socket()).await?;
    let mut stream = client
        .tail_logs(pb::TailLogsRequest {
            app,
            release,
            follow: args.follow,
            lines: args.lines,
            access: args.access,
        })
        .await
        .map_err(explain)?
        .into_inner();

    while let Some(line) = stream.next().await {
        let line = line.map_err(explain)?.line;

        if args.access && !args.json {
            println!("{}", format_access_line(&line));
        } else {
            println!("{line}");
        }
    }

    Ok(())
}

/// Reopen every log file after an external rotation.
pub async fn logs_reopen(workers: bool, socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let reopened = client
        .reopen_logs(pb::ReopenLogsRequest { workers })
        .await
        .map_err(explain)?
        .into_inner();
    println!(
        "muru: reopened the log files: {}{} host{}{}",
        if reopened.edge { "the edge and " } else { "" },
        reopened.hosts,
        if reopened.hosts == 1 { "" } else { "s" },
        if workers { " and their workers" } else { "" }
    );
    Ok(())
}

/// An access log record as the compact line a development run prints,
/// with the release, host, group and mount when the record names them.
/// A line that is not a record is printed as it is.
fn format_access_line(line: &str) -> String {
    let Ok(serde_json::Value::Object(record)) = serde_json::from_str::<serde_json::Value>(line)
    else {
        return line.to_owned();
    };

    let text = |key: &str| record.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let number = |key: &str| record.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let millis = |key: &str| {
        muru_edge::access::format_duration(Duration::from_secs_f64(number(key) / 1000.0))
    };
    let bytes =
        muru_edge::access::format_bytes(record.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0));
    let time = text("time");
    let clock = time.get(11..23).unwrap_or(time);
    let mut out = format!(
        "{clock} {} {} {} {bytes} {}",
        number("status") as u64,
        text("method"),
        text("target"),
        millis("total_ms")
    );

    if !text("app").is_empty() {
        out.push_str(&format!(" release={}@{}", text("app"), text("release")));
    }

    if !text("host").is_empty() {
        out.push_str(&format!(" host={}", text("host")));
    }

    if record.get("worker").is_some_and(|v| !v.is_null()) {
        out.push_str(&format!(
            " queue={} app={}",
            millis("queue_ms"),
            millis("app_ms")
        ));
    }

    if !text("reason").is_empty() {
        out.push_str(&format!(" reason={}", text("reason")));
    }

    for key in ["group", "mount"] {
        if !text(key).is_empty() && text(key) != "default" {
            out.push_str(&format!(" {key}={}", text(key)));
        }
    }

    if let Some(worker) = record.get("worker").and_then(|v| v.as_u64()) {
        out.push_str(&format!(" worker={worker}"));
    }

    out.push_str(&format!(" id={}", text("request_id")));
    out
}

fn format_event(event: &pb::Event) -> String {
    let seconds = event.time_unix_ms / 1000;
    let time = time::OffsetDateTime::from_unix_timestamp(seconds as i64)
        .map(|time| {
            format!(
                "{:02}:{:02}:{:02}",
                time.hour(),
                time.minute(),
                time.second()
            )
        })
        .unwrap_or_default();
    format!(
        "{:>6} {time} {:<8} {:<20} {:<24} {}",
        event.sequence, event.level, event.r#type, event.subject, event.message
    )
}

fn split_release(text: &str) -> anyhow::Result<(&str, &str)> {
    text.split_once('@')
        .filter(|(app, id)| !app.is_empty() && !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{text:?} is not a release: expected app@release"))
}

fn or_dash(text: &str) -> &str {
    if text.is_empty() { "-" } else { text }
}

/// A gRPC status as a plain sentence.
fn explain(status: tonic::Status) -> anyhow::Error {
    anyhow::anyhow!("{}", status.message())
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 checks for the process and sends nothing.

    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Read the daemon configuration file, which need not exist.
fn read_config(path: &Path) -> anyhow::Result<muru_config::Document> {
    if !path.is_file() {
        return Ok(muru_config::Document::default());
    }

    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The metrics address: the flag, else the [metrics] table's listen
/// address, else none.
fn metrics_from(
    document: &muru_config::Document,
    flag: Option<SocketAddr>,
) -> anyhow::Result<Option<SocketAddr>> {
    if flag.is_some() {
        return Ok(flag);
    }

    let listen = document
        .metrics
        .as_ref()
        .and_then(|metrics| metrics.listen.as_deref());

    match listen {
        Some(address) => Ok(Some(address.parse().map_err(|_| {
            anyhow::anyhow!(
                "[metrics] listen = {address:?} is not an address such as 127.0.0.1:9464"
            )
        })?)),
        None => Ok(None),
    }
}

/// The dashboard address: the flag, else the [dashboard] table's listen
/// address, else none; loopback only, as section 10.7 has it.
fn dashboard_from(
    document: &muru_config::Document,
    flag: Option<SocketAddr>,
) -> anyhow::Result<Option<SocketAddr>> {
    let address = match flag {
        Some(address) => Some(address),
        None => document
            .dashboard
            .as_ref()
            .and_then(|dashboard| dashboard.listen.as_deref())
            .map(|address| {
                address.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "[dashboard] listen = {address:?} is not an address such as 127.0.0.1:8090"
                    )
                })
            })
            .transpose()?,
    };

    if let Some(address) = address {
        anyhow::ensure!(
            address.ip().is_loopback(),
            "the dashboard is only served on loopback addresses, not {address}"
        );
    }

    Ok(address)
}

/// The event sinks of the configuration file, checked, with their
/// secrets read.
fn sinks_from(
    document: &muru_config::Document,
) -> anyhow::Result<Vec<muru_controller::sinks::Webhook>> {
    let mut sinks = Vec::new();

    for sink in document.events.iter().flat_map(|events| &events.sink) {
        let webhook = muru_controller::sinks::Webhook::from_config(sink)
            .map_err(|error| anyhow::anyhow!("[[events.sink]]: {error}"))?;

        if sinks
            .iter()
            .any(|seen: &muru_controller::sinks::Webhook| seen.name == webhook.name)
        {
            anyhow::bail!("[[events.sink]]: two sinks are named {}", webhook.name);
        }

        sinks.push(webhook);
    }

    Ok(sinks)
}

/// The listeners of the configuration file, or plain HTTP on 127.0.0.1:8080.
fn listeners_from(document: &muru_config::Document) -> anyhow::Result<Vec<(SocketAddr, bool)>> {
    if document.listener.is_empty() {
        return Ok(vec![("127.0.0.1:8080".parse()?, false)]);
    }

    let mut listeners = Vec::new();

    for listener in &document.listener {
        let address = listener
            .address
            .parse()
            .with_context(|| format!("listener address {:?}", listener.address))?;
        listeners.push((address, listener.tls));
    }

    Ok(listeners)
}

/// The edge limits from the [limits] table, with the defaults for the rest.
fn limits_from(document: &muru_config::Document) -> anyhow::Result<muru_edge::Limits> {
    let mut limits = muru_edge::Limits::default();
    let Some(table) = &document.limits else {
        return Ok(limits);
    };

    if let Some(size) = &table.max_body_size {
        limits.max_body_size = muru_config::parse_size(size).map_err(anyhow::Error::msg)?;
    }

    if let Some(size) = &table.max_header_bytes {
        limits.max_header_bytes =
            usize::try_from(muru_config::parse_size(size).map_err(anyhow::Error::msg)?)?;
    }

    if let Some(count) = table.max_headers {
        limits.max_headers = count;
    }

    if let Some(count) = table.max_connections {
        anyhow::ensure!(count > 0, "[limits] max_connections must be at least 1");
        limits.max_connections = count;
    }

    if let Some(count) = table.max_connections_per_client {
        limits.max_connections_per_client = count;
    }

    if let Some(count) = table.max_requests_per_connection {
        limits.max_requests_per_connection = count;
    }

    if let Some(size) = &table.spool_quota {
        limits.spool_quota = muru_config::parse_size(size).map_err(anyhow::Error::msg)?;
    }

    if let Some(count) = table
        .http2
        .as_ref()
        .and_then(|http2| http2.max_concurrent_streams)
    {
        anyhow::ensure!(
            count > 0,
            "[limits.http2] max_concurrent_streams must be at least 1"
        );
        limits.http2_max_concurrent_streams = count;
    }

    for (value, field) in [
        (&table.header_read_timeout, &mut limits.header_read_timeout),
        (&table.queue_timeout, &mut limits.queue_timeout),
        (&table.request_deadline, &mut limits.request_deadline),
        (&table.body_idle_timeout, &mut limits.body_idle_timeout),
        (
            &table.response_idle_timeout,
            &mut limits.response_idle_timeout,
        ),
        (&table.keep_alive_timeout, &mut limits.keep_alive_timeout),
        (
            &table.keep_alive_timeout_proxy,
            &mut limits.keep_alive_timeout_proxy,
        ),
    ] {
        if let Some(text) = value {
            *field = muru_config::parse_duration(text).map_err(anyhow::Error::msg)?;
        }
    }
    Ok(limits)
}
