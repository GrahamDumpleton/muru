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
    /// Where the edge writes the access log, or none.
    pub access_log: Option<PathBuf>,
    /// The executable to spawn edges from, normally the daemon's own.
    pub executable: PathBuf,
}

/// A request for the reconciler, answered when it has been carried out.
pub enum Command {
    /// Create a release and apply the blue/green strategy.
    Deploy {
        /// The application.
        app: String,
        /// The spec, resolved by the API from the request and the source.
        spec: ReleaseSpec,
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
    /// Stop the daemon: detaching, with hosts and the edge left serving
    /// for the next daemon to re-adopt, or draining everything first.
    Shutdown {
        /// Drain and stop every release and the edge before exiting.
        drain: bool,
        /// Where the acknowledgement goes, before the daemon exits.
        done: oneshot::Sender<()>,
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
    running: HashMap<(String, String), Running>,
    /// The release each application's traffic goes to, as the edge has
    /// been told; the store catches up once a switch has happened.
    serving: HashMap<String, String>,
    /// The application whose workers take requests when no host is
    /// declared: the last one traffic was switched to.
    default_app: Option<String>,
}

/// Run the daemon until it is told to stop: bind the edge, serve the admin
/// API, start the releases that were serving, then reconcile commands as
/// they arrive.
pub async fn run(config: Config) -> anyhow::Result<()> {
    // A [proxy] table the edge would refuse is refused here, before
    // anything is bound.

    let proxy = config.proxy.as_ref().map(proxy_message);

    if let Some(proxy) = &proxy {
        muru_edge::proxy::Trust::from_snapshot(proxy)
            .map_err(|error| anyhow::anyhow!("[proxy]: {error}"))?;
    }

    let locations = config.locations;

    for location in [&locations.state, &locations.run, &locations.logs] {
        std::fs::create_dir_all(&location.path)
            .with_context(|| format!("creating {}", location.path.display()))?;
    }

    let store = Arc::new(Store::open(&locations.state_store())?);
    let events = Events::new(Arc::clone(&store));

    // An edge a previous daemon left running is taken over, with its
    // listener handed back. Otherwise the daemon binds the listener and
    // keeps its own copy, so the edge process can be replaced while
    // connections wait in the backlog; the edge is up before any release,
    // answering 503 until one serves.

    let control = locations.run.path.join("edge.sock");
    let listener_socket = locations.run.path.join("edge-listener.sock");
    let edge_log = locations.logs.path.join("edge.log");

    let adopted = match read_record::<EdgeRecord>(&locations.run.path.join("edge.toml")) {
        Some(record) => EdgeProcess::adopt(
            record.pid,
            config.executable.clone(),
            record.control,
            record.listener_socket,
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

            EdgeProcess::spawn(EdgeSpec {
                executable: config.executable,
                listeners,
                control,
                listener_socket,
                log: edge_log,
                access_log: config.access_log,
                limits: config.limits,
            })
            .await?
        }
    };

    let listeners = edge.listeners();
    write_record(
        &locations.run.path.join("edge.toml"),
        &EdgeRecord {
            pid: edge.pid().unwrap_or(0),
            control: edge.control().to_path_buf(),
            listener_socket: edge.listener_socket().to_path_buf(),
            listeners: listeners
                .iter()
                .map(|(address, tls)| ListenerRecord {
                    address: *address,
                    tls: *tls,
                })
                .collect(),
        },
    );

    // The admin API on its socket.

    let (commands, mut inbox) = mpsc::channel::<Command>(16);
    let info = Arc::new(Info {
        locations: locations.clone(),
        listeners: listeners.clone(),
        started_unix_ms: crate::model::now_unix_ms(),
        edge_pid: std::sync::atomic::AtomicU32::new(edge.pid().unwrap_or(0)),
    });
    let handle = Handle {
        store: Arc::clone(&store),
        events: events.clone(),
        commands,
        info: Arc::clone(&info),
    };
    let socket = locations.admin_socket();
    let (api_stop, api_stopped) = oneshot::channel();
    let api = crate::api::serve(socket.clone(), handle, api_stopped).await?;
    let mut api = tokio::spawn(api);
    events.emit(
        "daemon.started",
        "daemon",
        "info",
        &format!(
            "serving on {}, admin socket {}",
            listeners
                .iter()
                .map(|(address, tls)| format!(
                    "{}://{address}",
                    if *tls { "https" } else { "http" }
                ))
                .collect::<Vec<_>>()
                .join(" and "),
            socket.display()
        ),
    );

    let mut daemon = Daemon {
        store,
        events,
        locations,
        edge,
        info,
        logs: Logs::default(),
        proxy,
        running: HashMap::new(),
        serving: HashMap::new(),
        default_app: None,
    };
    daemon.restore().await;

    // Commands are carried out one at a time; a host dying in between is
    // noticed by the next pass.

    let mut drain_on_exit = true;

    let outcome = loop {
        tokio::select! {
            command = inbox.recv() => match command {
                Some(Command::Shutdown { drain, done }) => {
                    drain_on_exit = drain;
                    let _ = done.send(());
                    break Ok(());
                }
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
    // everything down with it.

    if drain_on_exit {
        daemon.stop_all().await;
        daemon.edge.stop().await;
        let _ = std::fs::remove_file(daemon.locations.run.path.join("edge.toml"));
        daemon
            .events
            .emit("daemon.stopped", "daemon", "info", "stopped");
    } else {
        daemon.detach();
        daemon.events.emit(
            "daemon.detached",
            "daemon",
            "info",
            "detached; hosts and the edge keep serving",
        );
    }

    // The API answers the Shutdown call that brought the daemon here
    // before the socket goes.

    let _ = api_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), &mut api).await;
    let _ = std::fs::remove_file(&socket);
    outcome
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
                    },
                );
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
                let outcome = self.deploy(&app, spec, id, retain, created).await;
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
            Command::Shutdown { .. } => {}
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
            },
        );
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

    /// Where a release's host writes its output.
    fn host_log(&self, app: &str, id: &str) -> PathBuf {
        let dir = self.locations.logs.path.join("apps").join(app);
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{id}.log"))
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
fn clock(unix_ms: u64) -> String {
    let seconds = (unix_ms / 1000) % 86_400;
    format!("{:02}:{:02} UTC", seconds / 3600, (seconds % 3600) / 60)
}
