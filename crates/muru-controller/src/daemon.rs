//! The daemon: one long-running process that holds desired state, drives
//! releases toward it and serves the admin API (design specification
//! sections 9.1, 9.5, 10.1 and 13.3).
//!
//! This is the first slice: one node, one edge process (ADR 0002) on a
//! listener the daemon binds and hands down, applications deployed from
//! source directories with the blue/green strategy (start the new release,
//! move traffic when it is ready, drain the previous one), rollback to the
//! previous release, drains and events. Re-adoption of hosts across daemon
//! restarts, retention of drained releases and the rest of phase 2 follow.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use muru_proto::edge::v1 as pb;
use tokio::sync::{mpsc, oneshot};

use crate::edges::{EdgeProcess, EdgeSpec};
use crate::events::Events;
use crate::host::{HostControl, HostMessage, WorkerReply};
use crate::host::{HostProcess, HostSpec};
use crate::locations::Locations;
use crate::logs::Logs;
use crate::model::{Release, ReleaseSpec, ReleaseState, valid_name};
use crate::store::Store;

/// How long a release may take from its host starting to every worker
/// listening (section 9.1 startup timeout, provisional).
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// How long requests in flight on a drained release may take to finish
/// before its workers are stopped anyway (section 9.5 drain_timeout).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a superseded release stays warm (section 9.1), provisional
/// (section 19.2).
pub const RETENTION: Duration = Duration::from_secs(30 * 60);

/// What the daemon runs with.
pub struct Config {
    /// Where everything lives.
    pub locations: Locations,
    /// The listeners, each with whether it carries TLS; the first is the
    /// main one.
    pub listeners: Vec<(SocketAddr, bool)>,
    /// Timeouts and limits for the edge.
    pub limits: muru_edge::Limits,
    /// Trusted proxies and the headers taken from them (section 4.7).
    pub proxy: Option<muru_config::Proxy>,
    /// How responses are compressed (section 4.17).
    pub compression: Option<muru_config::Compression>,
    /// Where the edge writes the access log, or none.
    pub access_log: Option<PathBuf>,
    /// Where the Prometheus exposition is served, or nowhere.
    pub metrics: Option<SocketAddr>,
    /// Where the dashboard and JSON gateway are served, or nowhere.
    pub dashboard: Option<SocketAddr>,
    /// The webhooks events are delivered to (section 11.5).
    pub sinks: Vec<crate::sinks::Webhook>,
    /// The executable to spawn edges from, normally the daemon's own.
    pub executable: PathBuf,
    /// The arguments to re-execute the daemon with on an upgrade (section
    /// 13.4): its own, with the flag that names the version it ran before.
    pub upgrade_arguments: Vec<std::ffi::OsString>,
    /// The version this daemon ran before it re-executed itself for an
    /// upgrade, or none when it started afresh.
    pub upgraded_from: Option<String>,
}

/// An upgrade the daemon is about to carry out (section 13.4).
#[derive(Debug, Clone)]
pub struct Upgrade {
    /// The installed binary the daemon re-executes from.
    pub executable: PathBuf,
    /// The version that binary reports.
    pub version: String,
}

/// A request for the reconciler, answered when it has been carried out.
pub enum Command {
    /// Everything the dashboard shows (section 10.7).
    Dashboard {
        /// Answered with the data.
        done: oneshot::Sender<muru_dashboard::NodeData>,
    },
    /// The node's metrics as the Prometheus text exposition.
    Metrics {
        /// Answered with the text.
        done: oneshot::Sender<String>,
    },
    /// Create a release and apply the blue/green strategy.
    Deploy {
        /// The application.
        app: String,
        /// The spec, resolved by the API from the request and the source,
        /// boxed to keep the command small.
        spec: Box<ReleaseSpec>,
        /// The release id, or none to generate one.
        id: Option<String>,
        /// How long the superseded release stays warm; zero stops it.
        retain: Duration,
        /// Where the release record goes as soon as it exists.
        created: oneshot::Sender<Result<Release, String>>,
        /// Where the outcome goes once the release serves or has failed.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Restore traffic to the application's previous release.
    Rollback {
        /// The application.
        app: String,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Take a release out of traffic and stop it.
    Drain {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Push a fresh snapshot: the hosts or routes changed.
    Reconfigure {
        /// Where the acknowledgement goes.
        done: oneshot::Sender<Result<(), String>>,
    },
    /// Re-read the configuration file for what can change while running
    /// (section 13.3).
    Reload {
        /// Answered with what was reloaded, or why it was refused.
        done: oneshot::Sender<Result<String, String>>,
    },
    /// Reopen every log file after an external rotation (section 11.3):
    /// the daemon's own, the edge's access log and each host's, and with
    /// `workers` the application's own files through on_reopen_logs.
    ReopenLogs {
        /// Also pass the request on to the workers.
        workers: bool,
        /// Where the count of what was told goes.
        done: oneshot::Sender<Reopened>,
    },
    /// Stop the daemon: detaching, with hosts and the edge left serving
    /// for the next daemon to re-adopt, or draining everything first.
    Shutdown {
        /// Drain and stop every release and the edge before exiting.
        drain: bool,
        /// Where the acknowledgement goes, before the daemon exits.
        done: oneshot::Sender<()>,
    },
    /// Re-execute the daemon from the installed binary and replace the
    /// edge (section 13.4).
    Upgrade {
        /// Answered with what is about to happen, before the daemon
        /// re-executes, or why it will not.
        done: oneshot::Sender<Result<Upgrade, String>>,
    },
    /// A host's control connection is up (section 5.9).
    HostAttached {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The connection, for acknowledgements.
        control: HostControl,
    },
    /// A host said something over its control connection.
    HostMessage {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// What it said.
        message: HostMessage,
    },
    /// Ask a release's workers something (section 11.7).
    Debug {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The operation: status, stacks, tasks, inspect or logging.
        op: String,
        /// One worker, or every worker.
        worker: Option<u32>,
        /// The operation's arguments.
        args: serde_json::Value,
        /// Answered with each worker's reply, or why none could be asked.
        done: oneshot::Sender<Result<Vec<WorkerReply>, String>>,
    },
}

/// What a previous daemon left behind about its edge, for re-adoption
/// (section 10.2), in the runtime directory.
#[derive(serde::Serialize, serde::Deserialize)]
struct EdgeRecord {
    pid: u32,
    control: PathBuf,
    listener_socket: PathBuf,
    #[serde(default)]
    generation: u32,
    #[serde(default)]
    listeners: Vec<ListenerRecord>,
}

/// One of the edge's listeners, for re-adoption.
#[derive(serde::Serialize, serde::Deserialize)]
struct ListenerRecord {
    address: SocketAddr,
    tls: bool,
}

/// What a previous daemon left behind about a release's host, in the
/// release's run directory.
#[derive(serde::Serialize, serde::Deserialize)]
struct HostRecord {
    pid: u32,
    sockets: Vec<PathBuf>,
    #[serde(default)]
    groups: Vec<GroupRecord>,
}

/// A named worker group's sockets and paths, for re-adoption and the
/// snapshot.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct GroupRecord {
    name: String,
    paths: Vec<String>,
    sockets: Vec<PathBuf>,
}

/// What the API needs to talk to a running daemon.
#[derive(Clone)]
pub struct Handle {
    /// The store, for reads.
    pub store: Arc<Store>,
    /// The event stream.
    pub events: Events,
    /// Where commands go.
    pub commands: mpsc::Sender<Command>,
    /// The daemon's locations and listener, for status.
    pub info: Arc<Info>,
}

/// Facts about the running daemon, for status.
pub struct Info {
    /// Where everything lives.
    pub locations: Locations,
    /// The addresses the edge serves, each with whether it carries TLS.
    pub listeners: Vec<(SocketAddr, bool)>,
    /// When the daemon started, Unix milliseconds.
    pub started_unix_ms: u64,
    /// The edge process's pid, for status.
    pub edge_pid: std::sync::atomic::AtomicU32,
    /// Where the Prometheus exposition is served, or nowhere.
    pub metrics: Option<SocketAddr>,
    /// Where the dashboard is served, or nowhere.
    pub dashboard: Option<SocketAddr>,
}

/// What a ReopenLogs command reached.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reopened {
    /// Whether the edge was told.
    pub edge: bool,
    /// How many hosts were told.
    pub hosts: u32,
}

/// A running release: its host and the sockets its workers listen on,
/// and, once superseded, when its retention ends.
struct Running {
    host: HostProcess,
    /// The default group's sockets.
    sockets: Vec<PathBuf>,
    /// The named groups' sockets and paths.
    groups: Vec<GroupRecord>,
    retained_until: Option<Instant>,
    /// The host's control connection, once it is up.
    control: Option<HostControl>,
    /// What the workers last reported, for metrics.
    status: Vec<crate::host::WorkerStatus>,
}

/// The daemon's state while it runs.
struct Daemon {
    store: Arc<Store>,
    events: Events,
    locations: Locations,
    edge: EdgeProcess,
    info: Arc<Info>,
    logs: Logs,
    /// The [proxy] table of the configuration file, for every snapshot.
    proxy: Option<pb::Proxy>,
    /// The [compression] table, for every snapshot.
    compression: pb::Compression,
    running: HashMap<(String, String), Running>,
    /// The release each application's traffic goes to, as the edge has
    /// been told; the store catches up once a switch has happened.
    serving: HashMap<String, String>,
    /// The application whose workers take requests when no host is
    /// declared: the last one traffic was switched to.
    default_app: Option<String>,
    /// Whether this process writes to the daemon log file, as a daemon
    /// started in the background does, so a reopen applies to it too.
    owns_log: bool,
    /// Where the hosts' control messages are sent, into the same inbox
    /// as the commands.
    commands: mpsc::Sender<Command>,
    /// Requests to hosts awaiting their answers, by request id.
    asked: HashMap<u64, oneshot::Sender<Result<Vec<WorkerReply>, String>>>,
    /// The last request id given out.
    last_request: u64,
}

/// Run the daemon until it is told to stop: bind the edge, serve the admin
/// API, start the releases that were serving, then reconcile commands as
/// they arrive.
pub async fn run(mut config: Config) -> anyhow::Result<()> {
    // A [proxy] table the edge would refuse is refused here, before
    // anything is bound.

    let proxy = config.proxy.as_ref().map(proxy_message);

    if let Some(proxy) = &proxy {
        muru_edge::proxy::Trust::from_snapshot(proxy)
            .map_err(|error| anyhow::anyhow!("[proxy]: {error}"))?;
    }

    let compression = compression_message(config.compression.as_ref())
        .map_err(|error| anyhow::anyhow!("[compression]: {error}"))?;

    let metrics = config.metrics;
    let dashboard = config.dashboard;
    let sinks = std::mem::take(&mut config.sinks);
    let locations = config.locations;

    for location in [&locations.state, &locations.run, &locations.logs] {
        std::fs::create_dir_all(&location.path)
            .with_context(|| format!("creating {}", location.path.display()))?;
    }

    let store = Arc::new(Store::open(&locations.state_store())?);
    let events = Events::new(Arc::clone(&store));

    // Section 11.5: each sink delivers from its own cursor, so nothing
    // emitted from here on is missed by one.

    let deliveries: Vec<_> = sinks
        .into_iter()
        .map(|webhook| {
            tokio::spawn(crate::sinks::run(
                webhook,
                Arc::clone(&store),
                events.clone(),
            ))
        })
        .collect();

    // An edge a previous daemon left running is taken over, with its
    // listener handed back. Otherwise the daemon binds the listener and
    // keeps its own copy, so the edge process can be replaced while
    // connections wait in the backlog; the edge is up before any release,
    // answering 503 until one serves.

    let edge_log = locations.logs.path.join("edge.log");

    let adopted = match read_record::<EdgeRecord>(&locations.run.path.join("edge.toml")) {
        Some(record) => EdgeProcess::adopt(
            record.pid,
            config.executable.clone(),
            record.control,
            record.listener_socket,
            record.generation,
            record
                .listeners
                .iter()
                .map(|listener| listener.tls)
                .collect(),
            edge_log.clone(),
            config.access_log.clone(),
            config.limits.clone(),
        )
        .await
        .map_err(|error| tracing::info!(%error, "no edge to re-adopt"))
        .ok(),
        None => None,
    };

    let edge = match adopted {
        Some(edge) => {
            tracing::info!(pid = edge.pid().unwrap_or(0), "re-adopted the edge");
            edge
        }
        None => {
            anyhow::ensure!(!config.listeners.is_empty(), "no listener to serve on");
            let mut listeners = Vec::new();

            for (address, tls) in &config.listeners {
                let socket = std::net::TcpListener::bind(address)
                    .with_context(|| format!("cannot listen on {address}"))?;
                listeners.push(crate::edges::Listener { socket, tls: *tls });
            }

            let (control, listener_socket) = crate::edges::socket_paths(&locations.run.path, 1);
            EdgeProcess::spawn(EdgeSpec {
                executable: config.executable.clone(),
                listeners,
                control,
                listener_socket,
                generation: 1,
                log: edge_log,
                access_log: config.access_log,
                limits: config.limits,
            })
            .await?
        }
    };

    let listeners = edge.listeners();
    write_edge_record(&locations.run.path, &edge);

    // The admin API on its socket.

    let (commands, mut inbox) = mpsc::channel::<Command>(64);
    let handle_commands = commands.clone();
    let info = Arc::new(Info {
        locations: locations.clone(),
        listeners: listeners.clone(),
        started_unix_ms: crate::model::now_unix_ms(),
        edge_pid: std::sync::atomic::AtomicU32::new(edge.pid().unwrap_or(0)),
        metrics,
        dashboard,
    });
    let handle = Handle {
        store: Arc::clone(&store),
        events: events.clone(),
        commands,
        info: Arc::clone(&info),
    };

    // The metrics listener (section 11.1), on an internal address.

    let scraper = match metrics {
        Some(address) => {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .with_context(|| format!("cannot listen for metrics on {address}"))?;
            Some(tokio::spawn(crate::metrics::serve(
                listener,
                handle.commands.clone(),
            )))
        }
        None => None,
    };

    // The dashboard and gateway (section 10.7), on an internal address.

    let pages = match dashboard {
        Some(address) => {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .with_context(|| format!("cannot listen for the dashboard on {address}"))?;
            Some(tokio::spawn(crate::dashboard::serve(
                listener,
                handle.commands.clone(),
            )))
        }
        None => None,
    };

    let socket = locations.admin_socket();
    let (api_stop, api_stopped) = oneshot::channel();
    let api = crate::api::serve(socket.clone(), handle, api_stopped).await?;
    let mut api = tokio::spawn(api);
    let serving_on = listeners
        .iter()
        .map(|(address, tls)| format!("{}://{address}", if *tls { "https" } else { "http" }))
        .collect::<Vec<_>>()
        .join(" and ");

    match &config.upgraded_from {
        Some(before) => events.emit(
            "daemon.upgraded",
            "daemon",
            "info",
            &format!(
                "re-executed as version {} (was {before}) from {}, pid {}; serving on {serving_on}",
                env!("CARGO_PKG_VERSION"),
                config.executable.display(),
                std::process::id()
            ),
        ),
        None => events.emit(
            "daemon.started",
            "daemon",
            "info",
            &format!("serving on {serving_on}, admin socket {}", socket.display()),
        ),
    }

    let owns_log = writes_to(&locations.daemon_log());

    let mut daemon = Daemon {
        store,
        events,
        locations,
        edge,
        info,
        logs: Logs::default(),
        proxy,
        compression,
        running: HashMap::new(),
        serving: HashMap::new(),
        default_app: None,
        owns_log,
        commands: handle_commands,
        asked: HashMap::new(),
        last_request: 0,
    };
    daemon.restore().await;

    // Section 13.4: an upgraded daemon replaces the edge with one from
    // its own image once the hosts are re-adopted and the snapshot is
    // whole again.

    if config.upgraded_from.is_some() {
        daemon.replace_edge().await;
    }

    crate::notify::ready(&format!("serving on {serving_on}"));

    // Commands are carried out one at a time; a host dying in between is
    // noticed by the next pass. SIGTERM and SIGINT detach, as a service
    // manager's stop or a terminal's interrupt should (section 13.3), and
    // SIGHUP reloads the configuration file.

    let mut drain_on_exit = true;
    let mut upgrade_to: Option<Upgrade> = None;
    let mut reopen = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let outcome = loop {
        tokio::select! {
            _ = reopen.recv() => {
                daemon.reopen_logs(false);
            }
            _ = hangup.recv() => {
                if let Err(error) = daemon.reload().await {
                    tracing::warn!(%error, "reloading the configuration failed");
                }
            }
            _ = terminate.recv() => {
                drain_on_exit = false;
                break Ok(());
            }
            _ = interrupt.recv() => {
                drain_on_exit = false;
                break Ok(());
            }
            command = inbox.recv() => match command {
                Some(Command::Shutdown { drain, done }) => {
                    drain_on_exit = drain;
                    let _ = done.send(());
                    break Ok(());
                }
                Some(Command::Upgrade { done }) => match prepare_upgrade().await {
                    Ok(upgrade) => {
                        let _ = done.send(Ok(upgrade.clone()));
                        upgrade_to = Some(upgrade);
                        drain_on_exit = false;
                        break Ok(());
                    }
                    Err(error) => {
                        let _ = done.send(Err(error));
                    }
                },
                Some(command) => daemon.carry_out(command).await,
                None => break Ok(()),
            },
            served = &mut api => {
                break served.map_err(anyhow::Error::from).and_then(|result| result);
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                daemon.check_edge().await;
                daemon.check_hosts().await;
                daemon.expire_retained().await;
            }
        }
    };

    // Section 13.3: by default the daemon detaches, leaving hosts and the
    // edge serving for the next daemon to re-adopt; with drain it takes
    // everything down with it. For an upgrade (section 13.4) it detaches
    // the same way and then re-executes itself, so the next daemon is
    // this process with the installed binary's image.

    if upgrade_to.is_some() {
        crate::notify::notify("RELOADING=1\nSTATUS=upgrading");
    } else {
        crate::notify::stopping();
    }

    if drain_on_exit {
        daemon.stop_all().await;
        daemon.edge.stop().await;
        let _ = std::fs::remove_file(daemon.locations.run.path.join("edge.toml"));
        daemon
            .events
            .emit("daemon.stopped", "daemon", "info", "stopped");
    } else if upgrade_to.is_none() {
        daemon.detach();
        daemon.events.emit(
            "daemon.detached",
            "daemon",
            "info",
            "detached; hosts and the edge keep serving",
        );
    } else {
        daemon.detach();
    }

    // The API answers the Shutdown call that brought the daemon here
    // before the socket goes.

    let _ = api_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), &mut api).await;

    if let Some(scraper) = scraper {
        scraper.abort();
    }

    for delivery in deliveries {
        delivery.abort();
    }

    if let Some(pages) = pages {
        pages.abort();
    }

    let _ = std::fs::remove_file(&socket);

    if let Some(upgrade) = upgrade_to {
        // exec returns only when it failed; the hosts and the edge are
        // still serving, so this daemon leaves as a detached one does.

        let error = exec_daemon(&upgrade.executable, &config.upgrade_arguments);
        daemon.events.emit(
            "daemon.detached",
            "daemon",
            "error",
            &format!(
                "re-executing {} failed: {error}; hosts and the edge keep serving",
                upgrade.executable.display()
            ),
        );
        return Err(anyhow::anyhow!(
            "re-executing {} failed: {error}",
            upgrade.executable.display()
        ));
    }

    outcome
}

/// Write the edge's record for the next daemon.
fn write_edge_record(run_dir: &Path, edge: &EdgeProcess) {
    write_record(
        &run_dir.join("edge.toml"),
        &EdgeRecord {
            pid: edge.pid().unwrap_or(0),
            control: edge.control().to_path_buf(),
            listener_socket: edge.listener_socket().to_path_buf(),
            generation: edge.generation(),
            listeners: edge
                .listeners()
                .iter()
                .map(|(address, tls)| ListenerRecord {
                    address: *address,
                    tls: *tls,
                })
                .collect(),
        },
    );
}

/// Section 13.4: the binary an upgrade re-executes from. The daemon was
/// started as `muructl` by name or by path, so the installed binary is
/// the one that name finds on the PATH or that path names now; what the
/// kernel reports as the running executable may be a replaced file.
pub fn installed_executable() -> anyhow::Result<PathBuf> {
    let name = std::env::args_os()
        .next()
        .ok_or_else(|| anyhow::anyhow!("the daemon has no command name"))?;
    let name = PathBuf::from(name);

    let found = if name.components().count() > 1 {
        name.is_file().then_some(name)
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(&name))
                .find(|candidate| candidate.is_file())
        })
    };

    match found {
        Some(path) => Ok(path),
        None => std::env::current_exe().context("finding the installed executable"),
    }
}

/// Check the installed binary runs, and learn its version, before the
/// daemon commits to re-executing from it.
async fn prepare_upgrade() -> Result<Upgrade, String> {
    let executable = installed_executable().map_err(|error| format!("{error:#}"))?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(&executable)
            .arg("--version")
            .output(),
    )
    .await
    .map_err(|_| format!("{} did not report its version", executable.display()))?
    .map_err(|error| format!("{} cannot be run: {error}", executable.display()))?;

    if !output.status.success() {
        return Err(format!(
            "{} failed to report its version: {}",
            executable.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let version = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_owned();

    if version.is_empty() {
        return Err(format!("{} reported no version", executable.display()));
    }

    Ok(Upgrade {
        executable,
        version,
    })
}

/// Replace this process's image with the installed binary, keeping the
/// pid, the children and the environment. Returns only on failure.
fn exec_daemon(executable: &Path, arguments: &[std::ffi::OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;

    std::process::Command::new(executable)
        .args(arguments)
        .exec()
}

/// The [proxy] table as the snapshot carries it, with the defaults of
/// section 4.7 filled in: the four common header types, recursive
/// resolution, and the configured headers kept.
fn proxy_message(proxy: &muru_config::Proxy) -> pb::Proxy {
    let headers = if proxy.headers.is_empty() {
        muru_config::DEFAULT_PROXY_HEADERS
            .iter()
            .map(|header| (*header).to_owned())
            .collect()
    } else {
        proxy.headers.clone()
    };

    pb::Proxy {
        trusted: proxy.trusted.clone(),
        headers,
        recursive: proxy.recursive.unwrap_or(true),
        consume: proxy.forwarded_headers == Some(muru_config::ForwardedHeaders::Consume),
    }
}

/// The [compression] table as the snapshot carries it, checked: the
/// defaults of section 4.17 where the table is silent.
fn compression_message(
    table: Option<&muru_config::Compression>,
) -> Result<pb::Compression, String> {
    let Some(table) = table else {
        return Ok(muru_edge::compression::Compression::default().to_message());
    };

    let min_size = match &table.min_size {
        Some(text) => {
            muru_config::parse_size(text).map_err(|error| format!("min_size: {error}"))?
        }
        None => 0,
    };

    let message = pb::Compression {
        enabled: table.enabled.unwrap_or(true),
        encodings: table.encodings.clone(),
        min_size,
        types: table.types.clone(),
        level: table.level.clone().unwrap_or_default(),
    };

    // Checked the way the edge will read it, so a bad word is refused
    // here rather than in the edge's log.

    muru_edge::compression::Compression::from_snapshot(&message).map(|policy| policy.to_message())
}

/// A host's certificate chain and key as PEM text, for the snapshot.
fn read_pem_pair(certificate: &str, key: &str) -> anyhow::Result<(String, String)> {
    let chain =
        std::fs::read_to_string(certificate).with_context(|| format!("reading {certificate}"))?;
    let key = std::fs::read_to_string(key).with_context(|| format!("reading {key}"))?;

    Ok((chain, key))
}

/// Read a record a previous daemon wrote, if there is one it can decode.
fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Write a record for the next daemon.
fn write_record<T: serde::Serialize>(path: &Path, record: &T) {
    let written = toml::to_string(record)
        .map_err(std::io::Error::other)
        .and_then(|text| std::fs::write(path, text));

    if let Err(error) = written {
        tracing::error!(%error, path = %path.display(), "writing a record failed");
    }
}

/// Whether a process's command line names the path, which is how a host
/// record is checked against the process that has its pid now.
fn command_mentions(pid: u32, path: &Path) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output();

    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&path.display().to_string()),
        Err(_) => false,
    }
}

impl Daemon {
    /// Everything the dashboard shows (section 10.7): the node from the
    /// daemon's own facts, the releases from the store and the hosts,
    /// the requests and pools from the edge, and each serving release's
    /// log tail from its file.
    async fn dashboard_data(&mut self) -> muru_dashboard::NodeData {
        use muru_dashboard::{
            AppRow, EventRow, HostRow, LogPanel, NodeData, NodeRequestRow, ReleaseRow, RouteRow,
            Summary,
        };
        use muru_edge::access::{format_bytes, format_duration};

        let recent = match self.edge.recent(200).await {
            Ok(recent) => recent,
            Err(error) => {
                tracing::debug!(%error, "the edge's recent requests could not be read");
                pb::RecentRequests::default()
            }
        };

        let mut totals: Vec<u64> = recent
            .records
            .iter()
            .map(|record| record.total_us)
            .collect();
        totals.sort_unstable();
        let percentile = |fraction: f64| -> String {
            if totals.is_empty() {
                return "-".to_owned();
            }

            let index = ((totals.len() - 1) as f64 * fraction).round() as usize;
            format_duration(Duration::from_micros(totals[index]))
        };
        let summary = Summary {
            rate: format!(
                "{:.1}",
                recent.minute_requests as f64 / recent.minute_seconds.max(1) as f64
            ),
            p50: percentile(0.5),
            p95: percentile(0.95),
            errors: recent.minute_errors,
            in_flight: recent.pools.iter().map(|pool| pool.busy as usize).sum(),
        };

        let mut apps = Vec::new();

        for app in self.store.apps().unwrap_or_default() {
            let mut releases = Vec::new();

            for release in self.store.releases(Some(&app.name)).unwrap_or_default() {
                let workers = self
                    .running
                    .get(&(app.name.clone(), release.id.clone()))
                    .map_or(0, |running| {
                        running.sockets.len()
                            + running
                                .groups
                                .iter()
                                .map(|group| group.sockets.len())
                                .sum::<usize>()
                    });
                let pools: Vec<&pb::PoolState> = recent
                    .pools
                    .iter()
                    .filter(|pool| pool.app == app.name && pool.release == release.id)
                    .collect();
                releases.push(ReleaseRow {
                    id: release.id.clone(),
                    state: release.state.as_str().to_owned(),
                    serving: app.current.as_deref() == Some(release.id.as_str()),
                    message: release.message.clone(),
                    workers,
                    slots_busy: pools.iter().map(|pool| pool.busy as usize).sum(),
                    slots_total: pools.iter().map(|pool| pool.slots as usize).sum(),
                    waiting: pools.iter().map(|pool| pool.waiting as usize).sum(),
                    created: clock_hms(release.created_unix_ms),
                });
            }

            releases.reverse();
            apps.push(AppRow {
                name: app.name.clone(),
                current: app.current.clone().unwrap_or_default(),
                previous: app.previous.clone().unwrap_or_default(),
                releases,
            });
        }

        let hosts = self
            .store
            .hosts()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, host)| HostRow {
                name,
                names: host.names,
                listeners: host.listeners,
                tls: host.tls.is_some(),
                default: host.default,
            })
            .collect();
        let routes = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .map(|route| RouteRow {
                host: route.host,
                path: route.path,
                app: route.app,
            })
            .collect();
        let requests = recent
            .records
            .iter()
            .rev()
            .map(|record| NodeRequestRow {
                time: clock_hms(record.started_unix_ms),
                status: record.status as u16,
                status_class: (record.status / 100) as u16,
                reason: record.reason.clone(),
                method: record.method.clone(),
                target: record.target.clone(),
                release: if record.app.is_empty() {
                    "-".to_owned()
                } else {
                    format!("{}@{}", record.app, record.release)
                },
                host: record.host.clone(),
                bytes: format_bytes(record.bytes),
                total: format_duration(Duration::from_micros(record.total_us)),
                queue: format_duration(Duration::from_micros(record.queue_us)),
                application: format_duration(Duration::from_micros(record.application_us)),
                worker: if record.worker < 0 {
                    "-".to_owned()
                } else {
                    record.worker.to_string()
                },
                request_id: record.request_id.clone(),
            })
            .collect();
        let latest = self.store.last_event_sequence().unwrap_or(0);
        let mut events: Vec<EventRow> = self
            .store
            .events_after(latest.saturating_sub(50), 50)
            .unwrap_or_default()
            .into_iter()
            .map(|event| EventRow {
                sequence: event.sequence,
                time: clock_hms(event.time_unix_ms),
                level: event.level.to_owned(),
                kind: event.kind,
                subject: event.subject,
                message: event.message,
            })
            .collect();
        events.reverse();
        let mut logs = Vec::new();

        for app in &apps {
            if app.current.is_empty() {
                continue;
            }

            let path = self.locations.release_log(&app.name, &app.current);
            logs.push(LogPanel {
                app: app.name.clone(),
                release: app.current.clone(),
                lines: crate::logs::last_lines(&path, 40).await,
            });
        }

        NodeData {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            uptime: muru_dashboard::uptime(Duration::from_millis(
                crate::model::now_unix_ms().saturating_sub(self.info.started_unix_ms),
            )),
            listeners: self
                .info
                .listeners
                .iter()
                .map(|(address, tls)| {
                    format!("{}://{address}", if *tls { "https" } else { "http" })
                })
                .collect(),
            edge_pid: self
                .info
                .edge_pid
                .load(std::sync::atomic::Ordering::Relaxed),
            snapshot_version: self.edge.snapshot_version(),
            metrics: self
                .info
                .metrics
                .map(|address| format!("http://{address}/metrics")),
            summary,
            apps,
            hosts,
            routes,
            requests,
            events,
            logs,
        }
    }

    /// Section 13.3: re-read the configuration file for what can change
    /// while running, which is the [proxy] table; listeners, limits, the
    /// metrics listener and the event sinks are read at start.
    async fn reload(&mut self) -> Result<String, String> {
        let path = self.locations.config.path.clone();

        let document = match std::fs::read_to_string(&path) {
            Ok(text) => muru_config::Document::parse(&text)
                .map_err(|error| format!("{}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                muru_config::Document::default()
            }
            Err(error) => return Err(format!("reading {}: {error}", path.display())),
        };

        let proxy = document.proxy.as_ref().map(proxy_message);

        if let Some(proxy) = &proxy {
            muru_edge::proxy::Trust::from_snapshot(proxy)
                .map_err(|error| format!("[proxy]: {error}"))?;
        }

        let compression = compression_message(document.compression.as_ref())
            .map_err(|error| format!("[compression]: {error}"))?;
        let changed = proxy != self.proxy || compression != self.compression;
        self.proxy = proxy;
        self.compression = compression;

        if changed {
            self.push_snapshot().await?;
        }

        let message = format!(
            "reloaded {}: the [proxy] and [compression] tables {}; listeners, limits, metrics and event sinks are read at start",
            path.display(),
            if changed {
                "changed and the edge has them"
            } else {
                "are unchanged"
            }
        );
        self.events
            .emit("config.reloaded", "daemon", "info", &message);
        Ok(message)
    }

    /// The node's metrics: the edge's samples with the daemon's own
    /// gauges added, as the Prometheus text exposition.
    async fn metrics(&self) -> String {
        let mut samples = match self.edge.metrics().await {
            Ok(samples) => samples,
            Err(error) => {
                tracing::warn!(%error, "the edge's metrics could not be read");
                Vec::new()
            }
        };

        for ((app, id), running) in &self.running {
            let mut groups = vec![(
                muru_edge::routing::DEFAULT_GROUP.to_owned(),
                running.sockets.len(),
            )];

            for group in &running.groups {
                groups.push((group.name.clone(), group.sockets.len()));
            }

            for (group, workers) in groups {
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_workers_ready",
                    &[
                        ("app", app.as_str()),
                        ("release", id.as_str()),
                        ("group", group.as_str()),
                    ],
                    workers as f64,
                ));
            }

            // Sections 7.6 and 7.7: what each worker last reported.

            for worker in &running.status {
                let index = worker.index.to_string();
                let labels = [
                    ("app", app.as_str()),
                    ("release", id.as_str()),
                    ("group", worker.group.as_str()),
                    ("worker", index.as_str()),
                ];
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_worker_requests_total",
                    &labels,
                    worker.requests as f64,
                ));
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_worker_requests_in_flight",
                    &labels,
                    worker.in_flight as f64,
                ));

                for (measure, bytes) in [
                    ("rss", worker.rss),
                    ("pss", worker.pss),
                    ("uss", worker.uss),
                ] {
                    if let Some(bytes) = bytes {
                        samples.push(muru_edge::metrics::Sample::new(
                            "muru_worker_memory_bytes",
                            &[
                                ("app", app.as_str()),
                                ("release", id.as_str()),
                                ("group", worker.group.as_str()),
                                ("worker", index.as_str()),
                                ("measure", measure),
                            ],
                            bytes as f64,
                        ));
                    }
                }
            }
        }

        muru_edge::metrics::render(&muru_edge::metrics::aggregate(samples))
    }

    /// Re-adopt the hosts a previous daemon left running (section 10.2),
    /// and start again the release that served if its host is gone.
    async fn restore(&mut self) {
        let apps = match self.store.apps() {
            Ok(apps) => apps,
            Err(error) => {
                tracing::error!(%error, "reading the applications failed");
                return;
            }
        };

        for app in apps {
            // A release in a running state is taken over when its host is
            // still there and listening; otherwise it is gone with the
            // previous daemon.

            if let Ok(releases) = self.store.releases(Some(&app.name)) {
                for release in releases.iter().filter(|release| release.state.is_running()) {
                    if self.adopt_host(&release.app, &release.id) {
                        continue;
                    }

                    let _ = self.store.set_release_state(
                        &release.app,
                        &release.id,
                        ReleaseState::Stopped,
                        "the daemon restarted",
                    );
                }
            }

            let Some(current) = app.current.clone() else {
                continue;
            };
            if self
                .running
                .contains_key(&(app.name.clone(), current.clone()))
            {
                self.switch_to(&app.name, &current).await;
                continue;
            }

            match self.start(&app.name, &current).await {
                Ok(()) => self.switch_to(&app.name, &current).await,
                Err(error) => {
                    self.events.emit(
                        "release.failed",
                        &format!("{}@{current}", app.name),
                        "error",
                        &format!("could not restart after the daemon restarted: {error}"),
                    );
                }
            }
        }
    }

    /// Take over a release's host from its record, if it is still there.
    fn adopt_host(&mut self, app: &str, id: &str) -> bool {
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);
        let Some(record) = read_record::<HostRecord>(&run_dir.join("host.toml")) else {
            return false;
        };

        if !command_mentions(record.pid, &run_dir) {
            return false;
        }

        let group_sockets = record
            .groups
            .iter()
            .map(|group| (group.name.clone(), group.sockets.clone()))
            .collect();
        let adopted = HostProcess::adopt(
            record.pid,
            record.sockets.clone(),
            group_sockets,
            run_dir.join("r1"),
            1,
        );

        match adopted {
            Ok(host) => {
                let sockets = host.default_sockets();
                let retained_until = matches!(
                    self.store.release(app, id).map(|release| release.state),
                    Ok(ReleaseState::Retained)
                )
                .then(|| Instant::now() + RETENTION);
                self.running.insert(
                    (app.to_owned(), id.to_owned()),
                    Running {
                        host,
                        sockets,
                        groups: record.groups.clone(),
                        retained_until,
                        control: None,
                        status: Vec::new(),
                    },
                );
                self.attach_control(app, id);
                self.events.emit(
                    "release.adopted",
                    &format!("{app}@{id}"),
                    "info",
                    &format!("re-adopted the host (pid {})", record.pid),
                );
                true
            }
            Err(error) => {
                tracing::info!(%error, "release {app}@{id} could not be re-adopted");
                false
            }
        }
    }

    /// Leave every host and the edge running, for the next daemon.
    fn detach(&mut self) {
        let running = std::mem::take(&mut self.running);

        for (_, running) in running {
            running.host.release();
        }
    }

    async fn carry_out(&mut self, command: Command) {
        match command {
            Command::Deploy {
                app,
                spec,
                id,
                retain,
                created,
                done,
            } => {
                let outcome = self.deploy(&app, *spec, id, retain, created).await;
                let _ = done.send(outcome);
            }
            Command::Rollback { app, done } => {
                let _ = done.send(self.rollback(&app).await);
            }
            Command::Drain { app, id, done } => {
                let _ = done.send(self.drain_release(&app, &id).await);
            }
            Command::Reconfigure { done } => {
                let _ = done.send(self.push_snapshot().await);
            }
            Command::Reload { done } => {
                let _ = done.send(self.reload().await);
            }
            Command::ReopenLogs { workers, done } => {
                let _ = done.send(self.reopen_logs(workers));
            }
            Command::Metrics { done } => {
                let _ = done.send(self.metrics().await);
            }
            Command::Dashboard { done } => {
                let _ = done.send(self.dashboard_data().await);
            }
            Command::HostAttached { app, id, control } => {
                if let Some(running) = self.running.get_mut(&(app, id)) {
                    running.control = Some(control);
                }
            }
            Command::HostMessage { app, id, message } => {
                self.host_message(&app, &id, message).await;
            }
            Command::Debug {
                app,
                id,
                op,
                worker,
                args,
                done,
            } => {
                let control = self
                    .running
                    .get(&(app.clone(), id.clone()))
                    .and_then(|running| running.control.as_ref());

                match control {
                    Some(control) => {
                        self.last_request += 1;
                        control.request(self.last_request, &op, worker, &args);
                        self.asked.insert(self.last_request, done);
                    }
                    None => {
                        let _ = done.send(Err(format!(
                            "release {app}@{id} is not running with a host control connection"
                        )));
                    }
                }
            }
            Command::Shutdown { .. } | Command::Upgrade { .. } => {}
        }
    }

    /// Blue/green (section 9.2): record the release, start it, move traffic
    /// once it is ready, drain the release that served before.
    async fn deploy(
        &mut self,
        app: &str,
        spec: ReleaseSpec,
        id: Option<String>,
        retain: Duration,
        created: oneshot::Sender<Result<Release, String>>,
    ) -> Result<Release, String> {
        let record = self.create(app, spec, id);

        let release = match record {
            Ok(release) => release,
            Err(error) => {
                let _ = created.send(Err(error.clone()));
                return Err(error);
            }
        };

        let _ = created.send(Ok(release.clone()));
        let subject = format!("{app}@{}", release.id);
        self.events.emit(
            "release.created",
            &subject,
            "info",
            &format!("from {}", release.spec.source.display()),
        );

        self.start(app, &release.id).await?;

        // Traffic moves, then the previous release drains.

        let previous = self
            .store
            .app(app)
            .map_err(|error| error.to_string())?
            .current;
        self.switch_to(app, &release.id).await;
        self.store
            .set_app_releases(app, Some(&release.id), previous.as_deref())
            .map_err(|error| error.to_string())?;

        if let Some(previous) = previous.filter(|previous| previous != &release.id) {
            self.drain(app, &previous, retain).await;
        }

        self.store
            .release(app, &release.id)
            .map_err(|error| error.to_string())
    }

    /// Record a release from its spec, with a generated id unless one is given.
    fn create(&self, app: &str, spec: ReleaseSpec, id: Option<String>) -> Result<Release, String> {
        if !valid_name(app) {
            return Err(format!(
                "{app:?} is not a valid application name: lowercase letters, digits and hyphens, up to 63 characters"
            ));
        }

        self.store
            .ensure_app(app)
            .map_err(|error| error.to_string())?;

        let id = match id {
            Some(id) if valid_name(&id) => id,
            Some(id) => return Err(format!("{id:?} is not a valid release id")),
            None => self
                .store
                .next_release_id(app)
                .map_err(|error| error.to_string())?,
        };

        if self.store.release(app, &id).is_ok() {
            return Err(format!("release {app}@{id} already exists"));
        }

        self.store
            .create_release(app, &id, &spec)
            .map_err(|error| error.to_string())
    }

    /// Start a release's host and wait until every worker listens
    /// (pending or stopped to starting, warming, ready; or failed).
    async fn start(&mut self, app: &str, id: &str) -> Result<(), String> {
        let release = self
            .store
            .release(app, id)
            .map_err(|error| error.to_string())?;
        let subject = format!("{app}@{id}");

        if let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned())) {
            // Retained and warm: back to ready without a restart.

            running.retained_until = None;
            self.set_state(&release, ReleaseState::Ready, "");
            return Ok(());
        }

        self.set_state(&release, ReleaseState::Starting, "starting the host");
        let spec = &release.spec;
        let host_spec = HostSpec {
            python: spec.python.clone(),
            app: spec.entrypoint.clone(),
            interface: spec.interface,
            factory: spec.factory,
            processes: spec.processes as usize,
            threads: spec.threads as usize,
            sync_threads: spec.sync_threads as usize,
            concurrency: spec.concurrency as usize,
            preload: spec.preload,
            hooks: spec.hooks.clone(),
            chdir: Some(spec.source.clone()),
            tcp_base_port: None,
            single: false,
            detach_to: Some(self.host_log(app, id)),
            logging: spec.logging.clone(),
            max_requests: spec.max_requests,
            max_lifetime: spec.max_lifetime_ms.map(Duration::from_millis),
            hard_timeout: spec.timeouts.hard_timeout_ms.map(Duration::from_millis),
            hard_drain_grace: Duration::from_millis(spec.timeouts.hard_drain_grace_ms),
            shutdown_timeout: Duration::from_millis(spec.timeouts.shutdown_timeout_ms),
            memory: spec.memory.clone(),
            groups: spec
                .groups
                .iter()
                .map(|group| crate::host::HostGroup {
                    name: group.name.clone(),
                    processes: group.processes as usize,
                    threads: group.threads as usize,
                    sync_threads: group.sync_threads as usize,
                    concurrency: group.concurrency as usize,
                })
                .collect(),
            mounts: spec
                .mounts
                .iter()
                .map(|mount| crate::host::HostMount {
                    name: mount.name.clone(),
                    path: mount.path.clone(),
                    entrypoint: mount.entrypoint.clone(),
                    interface: mount.interface,
                })
                .collect(),
        };
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);
        let started = Instant::now();

        let mut host = match HostProcess::start(&host_spec, &run_dir, 1, &self.logs) {
            Ok(host) => host,
            Err(error) => {
                let message = format!("the host could not be started: {error:#}");
                self.set_state(&release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.failed", &subject, "error", &message);
                return Err(message);
            }
        };

        self.set_state(&release, ReleaseState::Warming, "waiting for the workers");

        if let Err(error) = host.wait_ready(START_TIMEOUT).await {
            let message = format!("{error:#}");
            host.stop().await;
            self.set_state(&release, ReleaseState::Failed, &message);
            self.events
                .emit("release.failed", &subject, "error", &message);
            return Err(message);
        }

        let sockets = host.default_sockets();
        let groups: Vec<GroupRecord> = host
            .group_sockets
            .iter()
            .map(|(name, sockets)| GroupRecord {
                name: name.clone(),
                paths: spec
                    .groups
                    .iter()
                    .find(|group| &group.name == name)
                    .map(|group| group.paths.clone())
                    .unwrap_or_default(),
                sockets: sockets.clone(),
            })
            .collect();
        write_record(
            &run_dir.join("host.toml"),
            &HostRecord {
                pid: host.pid().unwrap_or(0),
                sockets: host.sockets.clone(),
                groups: groups.clone(),
            },
        );
        self.running.insert(
            (app.to_owned(), id.to_owned()),
            Running {
                host,
                sockets,
                groups,
                retained_until: None,
                control: None,
                status: Vec::new(),
            },
        );
        self.attach_control(app, id);
        self.set_state(&release, ReleaseState::Ready, "");
        self.events.emit(
            "release.ready",
            &subject,
            "info",
            &format!(
                "{} workers ready in {:.1} s",
                spec.processes + spec.groups.iter().map(|group| group.processes).sum::<u32>(),
                started.elapsed().as_secs_f64()
            ),
        );
        Ok(())
    }

    /// Point the edge at a release's workers: the application's serving
    /// release in the snapshot, and the default workers when no host is
    /// declared.
    async fn switch_to(&mut self, app: &str, id: &str) {
        let subject = format!("{app}@{id}");

        if !self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            return;
        }

        self.serving.insert(app.to_owned(), id.to_owned());
        self.default_app = Some(app.to_owned());

        match self.push_snapshot().await {
            Ok(()) => {
                self.events.emit(
                    "traffic.switched",
                    &subject,
                    "notice",
                    "traffic moved to the release",
                );
            }
            Err(error) => {
                self.events.emit(
                    "release.failed",
                    &subject,
                    "error",
                    &format!("the edge could not take the release: {error}"),
                );
            }
        }
    }

    /// Section 4.4: desired state compiled for the edge. Each application
    /// contributes the workers of its serving release when that release
    /// runs; the hosts carry their certificates read from disk, so a
    /// replaced file takes effect with the next snapshot; the default
    /// workers are those of the application traffic was last switched to.
    fn build_snapshot(&mut self) -> pb::Snapshot {
        let mut apps = Vec::new();
        let mut serving: Vec<(&String, &String)> = self.serving.iter().collect();
        serving.sort();

        for (name, id) in serving {
            if let Some(running) = self.running.get(&(name.clone(), id.clone())) {
                apps.push(pb::AppWorkers {
                    app: name.clone(),
                    release: id.clone(),
                    sockets: running
                        .sockets
                        .iter()
                        .map(|socket| socket.display().to_string())
                        .collect(),
                    groups: running
                        .groups
                        .iter()
                        .map(|group| pb::Group {
                            name: group.name.clone(),
                            paths: group.paths.clone(),
                            sockets: group
                                .sockets
                                .iter()
                                .map(|socket| socket.display().to_string())
                                .collect(),
                        })
                        .collect(),
                    mounts: self
                        .store
                        .release(name, id)
                        .map(|release| {
                            release
                                .spec
                                .mounts
                                .iter()
                                .map(|mount| pb::Mount {
                                    name: mount.name.clone(),
                                    path: mount.path.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
        }

        if self
            .default_app
            .as_ref()
            .is_some_and(|name| !apps.iter().any(|app| &app.app == name))
        {
            self.default_app = None;
        }
        let default = self
            .default_app
            .as_ref()
            .and_then(|name| apps.iter().find(|app| &app.app == name))
            .or_else(|| apps.first());
        let worker_sockets = default.map(|app| app.sockets.clone()).unwrap_or_default();

        let mut hosts = Vec::new();

        for (name, host) in self.store.hosts().unwrap_or_default() {
            let (certificate_pem, key_pem) = match &host.tls {
                Some(tls) => {
                    match read_pem_pair(&tls.certificate, &tls.key) {
                        Ok(pair) => pair,
                        Err(error) => {
                            self.events.emit(
                            "certificate.unreadable",
                            &format!("host {name}"),
                            "error",
                            &format!("{error:#}; the host serves without TLS until the files are back"),
                        );
                            (String::new(), String::new())
                        }
                    }
                }
                None => (String::new(), String::new()),
            };

            hosts.push(pb::Host {
                name,
                names: host.names,
                listeners: host.listeners,
                certificate_pem,
                key_pem,
                https_redirect: host.https_redirect,
                canonical: host.canonical.unwrap_or_default(),
                use_canonical_name: host.use_canonical_name,
                default: host.default,
            });
        }

        let routes = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .map(|route| pb::Route {
                host: route.host,
                path: route.path,
                app: route.app,
            })
            .collect();

        pb::Snapshot {
            version: 0,
            worker_sockets,
            hosts,
            routes,
            apps,
            proxy: self.proxy.clone(),
            compression: Some(self.compression.clone()),
        }
    }

    /// Compile and push a snapshot.
    async fn push_snapshot(&mut self) -> Result<(), String> {
        let snapshot = self.build_snapshot();
        self.edge
            .apply(snapshot)
            .await
            .map_err(|error| format!("{error:#}"))
    }

    /// Section 9.5: with traffic already elsewhere, wait for the release's
    /// requests in flight to finish, then keep it warm for `retain` (section
    /// 9.1) or stop its host.
    async fn drain(&mut self, app: &str, id: &str, retain: Duration) {
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };

        if !self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            if release.state.is_running() {
                self.set_state(&release, ReleaseState::Stopped, "");
            }

            return;
        }

        // The previous pool is gone from the edge, so what is in flight on
        // these workers is what the edge had already dispatched; a short
        // wait lets it finish.

        self.set_state(
            &release,
            ReleaseState::Draining,
            "requests in flight are finishing",
        );
        self.events
            .emit("release.draining", &subject, "info", "draining");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // One retained release per application: an older one makes way.

        if !retain.is_zero() {
            let older: Vec<String> = self
                .running
                .iter()
                .filter(|((other_app, other_id), running)| {
                    other_app == app && other_id != id && running.retained_until.is_some()
                })
                .map(|((_, other_id), _)| other_id.clone())
                .collect();

            for other in older {
                self.stop_release(app, &other, "superseded again").await;
            }

            if let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned())) {
                running.retained_until = Some(Instant::now() + retain);
            }

            let until = crate::model::now_unix_ms() + retain.as_millis() as u64;
            self.set_state(
                &release,
                ReleaseState::Retained,
                &format!("warm until {}; a rollback to it is instant", clock(until)),
            );
            self.events.emit(
                "release.retained",
                &subject,
                "info",
                &format!("kept warm for {}", muru_config::format_duration(retain)),
            );
            return;
        }

        self.stop_release(app, id, "").await;
    }

    /// Stop a release's host and record it stopped.
    async fn stop_release(&mut self, app: &str, id: &str, why: &str) {
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };
        let Some(running) = self.running.remove(&(app.to_owned(), id.to_owned())) else {
            return;
        };

        self.set_state(&release, ReleaseState::Stopping, "stopping the host");
        let record = running.host.run_dir().join("..").join("host.toml");
        let stopped = tokio::time::timeout(DRAIN_TIMEOUT, running.host.stop()).await;

        if stopped.is_err() {
            tracing::warn!(%subject, "the host did not stop within the drain timeout");
        }

        let _ = std::fs::remove_file(record);
        self.set_state(&release, ReleaseState::Stopped, why);
        self.events
            .emit("release.stopped", &subject, "info", "stopped");
    }

    /// Stop the retained releases whose window has passed.
    async fn expire_retained(&mut self) {
        let now = Instant::now();
        let expired: Vec<(String, String)> = self
            .running
            .iter()
            .filter(|(_, running)| running.retained_until.is_some_and(|until| until <= now))
            .map(|(key, _)| key.clone())
            .collect();

        for (app, id) in expired {
            self.stop_release(&app, &id, "the retention window passed")
                .await;
        }
    }

    /// Section 9.6: start the previous release again if it is not running,
    /// move traffic to it, and drain the one that served.
    async fn rollback(&mut self, app: &str) -> Result<Release, String> {
        let record = self.store.app(app).map_err(|error| error.to_string())?;
        let Some(previous) = record.previous.clone() else {
            return Err(format!("{app} has no previous release to roll back to"));
        };
        let current = record.current.clone();

        self.start(app, &previous).await?;
        self.switch_to(app, &previous).await;
        self.store
            .set_app_releases(app, Some(&previous), current.as_deref())
            .map_err(|error| error.to_string())?;
        self.events.emit(
            "app.rolled-back",
            app,
            "notice",
            &format!("rolled back to {previous}"),
        );

        if let Some(current) = current.filter(|current| current != &previous) {
            self.drain(app, &current, RETENTION).await;
        }

        self.store
            .release(app, &previous)
            .map_err(|error| error.to_string())
    }

    /// Take a release out of traffic, if it serves, and stop it.
    async fn drain_release(&mut self, app: &str, id: &str) -> Result<Release, String> {
        let record = self.store.app(app).map_err(|error| error.to_string())?;
        self.store
            .release(app, id)
            .map_err(|error| error.to_string())?;

        if record.current.as_deref() == Some(id) {
            self.clear_traffic(app).await;
            self.store
                .set_app_releases(app, None, Some(id))
                .map_err(|error| error.to_string())?;
            self.events
                .emit("traffic.switched", app, "notice", "no release serves");
        }

        self.drain(app, id, Duration::ZERO).await;

        self.store
            .release(app, id)
            .map_err(|error| error.to_string())
    }

    /// A host that exited on its own fails its release; the release that
    /// serves is started again once.
    async fn check_hosts(&mut self) {
        let mut exited = Vec::new();

        for (key, running) in &mut self.running {
            if let Ok(Some(how)) = running.host.try_exited() {
                exited.push((key.clone(), how));
            }
        }

        for ((app, id), how) in exited {
            self.running.remove(&(app.clone(), id.clone()));
            let subject = format!("{app}@{id}");
            let message = format!("the host {how}");

            if let Ok(release) = self.store.release(&app, &id) {
                self.set_state(&release, ReleaseState::Failed, &message);
            }

            self.events
                .emit("release.failed", &subject, "error", &message);

            let serving = self
                .store
                .app(&app)
                .ok()
                .and_then(|record| record.current)
                .as_deref()
                == Some(id.as_str());

            if serving {
                self.clear_traffic(&app).await;
                self.events.emit(
                    "release.restarting",
                    &subject,
                    "warning",
                    "starting the release again",
                );

                if self.start(&app, &id).await.is_ok() {
                    self.switch_to(&app, &id).await;
                }
            }
        }
    }

    /// Push a snapshot without the application's workers: its serving
    /// release has just been unset or is being restarted.
    async fn clear_traffic(&mut self, app: &str) {
        self.serving.remove(app);

        if let Err(error) = self.push_snapshot().await {
            tracing::error!(%error, "pushing the snapshot failed");
        }
    }

    /// An edge that died is started again on the same listener with the
    /// last snapshot (section 2.4).
    async fn check_edge(&mut self) {
        match self.edge.check().await {
            Ok(false) => {}
            Ok(true) => {
                self.info.edge_pid.store(
                    self.edge.pid().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                write_edge_record(&self.locations.run.path, &self.edge);
                self.events.emit(
                    "edge.restarted",
                    "edge",
                    "warning",
                    "the edge process exited and was started again on the same listener",
                );
            }
            Err(error) => {
                self.events
                    .emit("edge.failed", "edge", "error", &format!("{error:#}"));
            }
        }
    }

    /// Section 5.9: connect to a running host's control socket in the
    /// background and feed what it says into the daemon's inbox.
    fn attach_control(&self, app: &str, id: &str) {
        let Some(running) = self.running.get(&(app.to_owned(), id.to_owned())) else {
            return;
        };
        let path = running.host.control_path();
        let commands = self.commands.clone();
        let (app, id) = (app.to_owned(), id.to_owned());

        tokio::spawn(async move {
            let (control, mut messages) = match HostControl::connect(&path).await {
                Ok(connected) => connected,
                Err(error) => {
                    tracing::info!(%error, "release {app}@{id}: no host control connection");
                    return;
                }
            };

            if commands
                .send(Command::HostAttached {
                    app: app.clone(),
                    id: id.clone(),
                    control,
                })
                .await
                .is_err()
            {
                return;
            }

            while let Some(message) = messages.recv().await {
                let sent = commands
                    .send(Command::HostMessage {
                        app: app.clone(),
                        id: id.clone(),
                        message,
                    })
                    .await;

                if sent.is_err() {
                    break;
                }
            }
        });
    }

    /// Act on what a host said: new worker sockets go into the record
    /// and, when the release serves, into the next snapshot, and are
    /// acknowledged; events are recorded under the release.
    async fn host_message(&mut self, app: &str, id: &str, message: HostMessage) {
        let key = (app.to_owned(), id.to_owned());

        match message {
            HostMessage::Workers {
                version,
                sockets,
                groups,
            } => {
                let Some(running) = self.running.get_mut(&key) else {
                    return;
                };
                running.sockets = sockets;

                for (name, group_sockets) in groups {
                    if let Some(group) = running.groups.iter_mut().find(|group| group.name == name)
                    {
                        group.sockets = group_sockets;
                    }
                }

                let mut all = running.sockets.clone();

                for group in &running.groups {
                    all.extend(group.sockets.iter().cloned());
                }

                write_record(
                    &running.host.run_dir().join("..").join("host.toml"),
                    &HostRecord {
                        pid: running.host.pid().unwrap_or(0),
                        sockets: all,
                        groups: running.groups.clone(),
                    },
                );

                if self.serving.get(app) == Some(&id.to_owned())
                    && let Err(error) = self.push_snapshot().await
                {
                    tracing::error!(%error, "pushing the snapshot after a worker change failed");
                    return;
                }

                if let Some(control) = self
                    .running
                    .get(&key)
                    .and_then(|running| running.control.as_ref())
                {
                    control.acknowledge(version);
                }
            }
            HostMessage::Status { workers } => {
                if let Some(running) = self.running.get_mut(&key) {
                    running.status = workers;
                }
            }
            HostMessage::Response { id, workers, error } => {
                if let Some(done) = self.asked.remove(&id) {
                    let _ = done.send(if error.is_empty() {
                        Ok(workers)
                    } else {
                        Err(error)
                    });
                }
            }
            HostMessage::Event {
                kind,
                level,
                message,
            } => {
                let level = match level.as_str() {
                    "error" => "error",
                    "warning" => "warning",
                    "notice" => "notice",
                    _ => "info",
                };

                self.events
                    .emit(&kind, &format!("{app}@{id}"), level, &message);
            }
        }
    }

    /// Section 13.4: replace the edge with one started from this image on
    /// the same listeners; the old one finishes its connections and exits.
    async fn replace_edge(&mut self) {
        let grace = self.edge.drain_grace();

        match self.edge.replace(grace).await {
            Ok(old_pid) => {
                let pid = self.edge.pid().unwrap_or(0);
                self.info
                    .edge_pid
                    .store(pid, std::sync::atomic::Ordering::Relaxed);
                write_edge_record(&self.locations.run.path, &self.edge);
                self.events.emit(
                    "edge.replaced",
                    "edge",
                    "info",
                    &format!(
                        "edge pid {old_pid} replaced by pid {pid} on the same listeners; the old edge finishes its connections and exits"
                    ),
                );
            }
            Err(error) => {
                self.events.emit(
                    "edge.failed",
                    "edge",
                    "error",
                    &format!(
                        "the edge could not be replaced: {error:#}; the old edge keeps serving"
                    ),
                );
            }
        }
    }

    /// Stop every host, for shutdown.
    async fn stop_all(&mut self) {
        let running = std::mem::take(&mut self.running);
        self.serving.clear();

        if let Err(error) = self.push_snapshot().await {
            tracing::error!(%error, "pushing the empty snapshot failed");
        }

        for ((app, id), running) in running {
            if let Ok(release) = self.store.release(&app, &id) {
                self.set_state(&release, ReleaseState::Stopping, "the daemon is stopping");
            }

            let _ = tokio::time::timeout(DRAIN_TIMEOUT, running.host.stop()).await;

            if let Ok(release) = self.store.release(&app, &id) {
                self.set_state(&release, ReleaseState::Stopped, "the daemon stopped");
            }
        }
    }

    /// Where a release's host writes its output, its directory created.
    fn host_log(&self, app: &str, id: &str) -> PathBuf {
        let path = self.locations.release_log(app, id);

        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        path
    }

    /// Section 11.3: after logrotate has renamed the files, every writer
    /// opens its file again by name. The daemon reopens its own when it
    /// was started in the background, the edge its access log on SIGUSR1,
    /// and each host its release log on SIGUSR1; SIGUSR2 makes a host
    /// pass SIGUSR1 on to its workers, whose on_reopen_logs hook handles
    /// files the application opened itself.
    fn reopen_logs(&mut self, workers: bool) -> Reopened {
        if self.owns_log {
            reopen_own_log(&self.locations.daemon_log());
        }

        let edge = self.edge.signal(libc::SIGUSR1);
        let mut hosts = 0;

        for running in self.running.values() {
            if running.host.signal(libc::SIGUSR1) {
                hosts += 1;
            }

            if workers {
                running.host.signal(libc::SIGUSR2);
            }
        }

        let message = format!(
            "reopened the log files: {}{hosts} host{}{}",
            if edge { "the edge and " } else { "" },
            if hosts == 1 { "" } else { "s" },
            if workers { " and their workers" } else { "" }
        );
        self.events.emit("logs.reopened", "logs", "info", &message);
        Reopened { edge, hosts }
    }

    fn set_state(&self, release: &Release, state: ReleaseState, message: &str) {
        if let Err(error) = self
            .store
            .set_release_state(&release.app, &release.id, state, message)
        {
            tracing::error!(%error, "recording a release state failed");
        }
    }
}

/// A wall clock time as HH:MM UTC, for messages.
/// A Unix time as HH:MM:SS, UTC.
fn clock_hms(unix_ms: u64) -> String {
    let seconds = (unix_ms / 1000) % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn clock(unix_ms: u64) -> String {
    let seconds = (unix_ms / 1000) % 86_400;
    format!("{:02}:{:02} UTC", seconds / 3600, (seconds % 3600) / 60)
}

/// Whether this process's standard output is the file at `path`, which
/// is how a daemon started in the background finds that it owns the
/// daemon log.
fn writes_to(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(current) = std::fs::metadata(path) else {
        return false;
    };

    // SAFETY: fstat on descriptor 1 of this process, into a zeroed struct
    // it fills; the descriptor stays open throughout.

    let (described, stat) = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        (libc::fstat(1, &mut stat) == 0, stat)
    };

    described
        && current.dev() as libc::dev_t == stat.st_dev
        && current.ino() as libc::ino_t == stat.st_ino
}

/// Open the daemon's log file again on standard output and error: once
/// logrotate has renamed it away, the lines belong in a new file at the
/// same path.
fn reopen_own_log(path: &std::path::Path) {
    use std::os::unix::io::AsRawFd as _;

    let Ok(log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };

    // SAFETY: dup2 of an open file onto the standard descriptors, which
    // stay valid; `log` closes its own descriptor when it drops.

    unsafe {
        libc::dup2(log.as_raw_fd(), 1);
        libc::dup2(log.as_raw_fd(), 2);
    }
}
