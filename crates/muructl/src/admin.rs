//! The commands that run or talk to the daemon (design specification
//! sections 10.4, 13.2 and 13.3, appendix A): `daemon run`, `daemon
//! start`, `daemon stop`, `daemon status`, `up`, `deploy`, `app list`,
//! `release list`, `release wait`, `rollback`, `drain`, `status`,
//! `events`, `apply`, `config export` and `config diff`.

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

    #[command(flatten)]
    locations: LocationArgs,
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
    muru_controller::daemon::run(muru_controller::daemon::Config {
        locations,
        listeners,
        limits,
        access_log,
        executable: std::env::current_exe().context("finding the muructl executable")?,
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

/// Stop the daemon through its API and wait for its process to go.
pub async fn daemon_stop(socket: SocketArgs, shutdown: bool) -> anyhow::Result<()> {
    let path = socket.socket();
    let mut client = client::connect(&path).await?;
    let status = client
        .get_status(pb::GetStatusRequest {})
        .await?
        .into_inner();
    client
        .shutdown(pb::ShutdownRequest { drain: shutdown })
        .await?;

    // The daemon removes its socket as it exits; a process that has exited
    // but not been reaped would still answer kill(0), so the socket is the
    // signal to wait for.

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(90) {
        let answering = match client::connect(&path).await {
            Ok(mut client) => client.get_status(pb::GetStatusRequest {}).await.is_ok(),
            Err(_) => false,
        };
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
    anyhow::bail!("the daemon (pid {}) did not exit", status.pid)
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
    println!("state    {}", status.state_dir);
    println!("run      {}", status.run_dir);
    println!("apps     {} ({} releases)", status.apps, status.releases);
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

    /// How long the release this one replaces stays warm for an instant
    /// rollback (default 30m; 0 stops it as soon as it has drained).
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    retain: Option<Duration>,

    #[command(flatten)]
    socket: SocketArgs,
}

/// Deploy a release.
pub async fn deploy(args: DeployArgs) -> anyhow::Result<()> {
    let source = args
        .source
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let source = source
        .canonicalize()
        .with_context(|| format!("{} does not exist", source.display()))?;
    let mut client = client::connect(&args.socket.socket()).await?;

    let request = pb::DeployRequest {
        app: args.app.clone(),
        source: source.display().to_string(),
        entrypoint: args.entrypoint.unwrap_or_default(),
        interface: args
            .interface
            .map(|value| value.as_str().to_owned())
            .unwrap_or_default(),
        python: args
            .python
            .map(|path| path.display().to_string())
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
        retain_ms: args.retain.map(|retain| retain.as_millis() as u64),
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
            if release.serving { " and serving" } else { "" }
        );
    }
    Ok(())
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
    println!("{:<20} {:<12} {:<12}", "APP", "CURRENT", "PREVIOUS");
    for app in apps {
        println!(
            "{:<20} {:<12} {:<12}",
            app.name,
            or_dash(&app.current_release),
            or_dash(&app.previous_release)
        );
    }
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
    println!("{:<16} {:<24} APP", "HOST", "PATH");
    for route in routes {
        println!("{:<16} {:<24} {}", route.host, route.path, route.app);
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
            format!("{}x{}", release.processes, release.threads),
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

        /// Write JSON rather than TOML.
        #[arg(long)]
        json: bool,

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
    json: bool,
    socket: SocketArgs,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.socket()).await?;
    let export = client
        .export_config(pb::ExportConfigRequest {
            app: app.unwrap_or_default(),
            all_releases,
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
    for (value, field) in [
        (&table.header_read_timeout, &mut limits.header_read_timeout),
        (&table.queue_timeout, &mut limits.queue_timeout),
        (&table.request_deadline, &mut limits.request_deadline),
        (&table.body_idle_timeout, &mut limits.body_idle_timeout),
        (
            &table.response_idle_timeout,
            &mut limits.response_idle_timeout,
        ),
    ] {
        if let Some(text) = value {
            *field = muru_config::parse_duration(text).map_err(anyhow::Error::msg)?;
        }
    }
    Ok(limits)
}
