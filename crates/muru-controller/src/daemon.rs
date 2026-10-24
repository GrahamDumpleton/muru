//! The daemon: one long-running process that holds desired state, drives
//! releases toward it and serves the admin API (design specification
//! sections 9.1, 9.5, 10.1 and 13.3).
//!
//! This is the first slice: one node, one edge embedded in the daemon
//! process, applications deployed from source directories with the
//! blue/green strategy (start the new release, move traffic when it is
//! ready, drain the previous one), rollback to the previous release, drains
//! and events. Separate edge processes, re-adoption of hosts across daemon
//! restarts, retention of drained releases and the rest of phase 2 follow.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tokio::sync::{mpsc, oneshot};

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

/// What the daemon runs with.
pub struct Config {
    /// Where everything lives.
    pub locations: Locations,
    /// The HTTP listener.
    pub listen: SocketAddr,
    /// Timeouts and limits for the edge.
    pub limits: muru_edge::Limits,
    /// Where the access log goes.
    pub access_log: muru_edge::access::Config,
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
    /// Stop the daemon.
    Shutdown {
        /// Where the acknowledgement goes, before the daemon exits.
        done: oneshot::Sender<()>,
    },
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
    /// The address the edge serves.
    pub listen: SocketAddr,
    /// When the daemon started, Unix milliseconds.
    pub started_unix_ms: u64,
}

/// A running release: its host and the sockets its workers listen on.
struct Running {
    host: HostProcess,
    sockets: Vec<PathBuf>,
}

/// The daemon's state while it runs.
struct Daemon {
    store: Arc<Store>,
    events: Events,
    locations: Locations,
    pools: Arc<muru_edge::Pools>,
    logs: Logs,
    running: HashMap<(String, String), Running>,
}

/// Run the daemon until it is told to stop: bind the edge, serve the admin
/// API, start the releases that were serving, then reconcile commands as
/// they arrive.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let locations = config.locations;
    for location in [&locations.state, &locations.run, &locations.logs] {
        std::fs::create_dir_all(&location.path)
            .with_context(|| format!("creating {}", location.path.display()))?;
    }
    let store = Arc::new(Store::open(&locations.state_store())?);
    let events = Events::new(Arc::clone(&store));

    // The edge is up before any release, answering 503 until one serves.
    let edge = muru_edge::Edge::bind(muru_edge::Config {
        listen: config.listen,
        sockets: Vec::new(),
        development: false,
        access_log: config.access_log,
        limits: config.limits,
        https: None,
    })
    .await?;
    let listen = edge.local_addr();
    let pools = edge.pools();
    let mut serving = tokio::spawn(edge.run());

    // The admin API on its socket.
    let (commands, mut inbox) = mpsc::channel::<Command>(16);
    let info = Arc::new(Info {
        locations: locations.clone(),
        listen,
        started_unix_ms: crate::model::now_unix_ms(),
    });
    let handle = Handle {
        store: Arc::clone(&store),
        events: events.clone(),
        commands,
        info,
    };
    let socket = locations.admin_socket();
    let api = crate::api::serve(socket.clone(), handle).await?;
    let mut api = tokio::spawn(api);
    events.emit(
        "daemon.started",
        "daemon",
        "info",
        &format!(
            "serving on http://{listen}, admin socket {}",
            socket.display()
        ),
    );

    let mut daemon = Daemon {
        store,
        events,
        locations,
        pools,
        logs: Logs::default(),
        running: HashMap::new(),
    };
    daemon.restore().await;

    // Commands are carried out one at a time; a host dying in between is
    // noticed by the next pass.
    let outcome = loop {
        tokio::select! {
            command = inbox.recv() => match command {
                Some(Command::Shutdown { done }) => {
                    let _ = done.send(());
                    break Ok(());
                }
                Some(command) => daemon.carry_out(command).await,
                None => break Ok(()),
            },
            served = &mut serving => {
                break served.map_err(anyhow::Error::from).and_then(|result| result);
            }
            served = &mut api => {
                break served.map_err(anyhow::Error::from).and_then(|result| result);
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => daemon.check_hosts().await,
        }
    };

    daemon.stop_all().await;
    let _ = std::fs::remove_file(&socket);
    daemon
        .events
        .emit("daemon.stopped", "daemon", "info", "stopped");
    outcome
}

impl Daemon {
    /// Start the releases that were serving when the last daemon stopped.
    /// Hosts are not re-adopted yet, so they are started afresh.
    async fn restore(&mut self) {
        let apps = match self.store.apps() {
            Ok(apps) => apps,
            Err(error) => {
                tracing::error!(%error, "reading the applications failed");
                return;
            }
        };

        for app in apps {
            // Whatever was running is gone with the previous daemon.
            if let Ok(releases) = self.store.releases(Some(&app.name)) {
                for release in releases.iter().filter(|release| release.state.is_running()) {
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

    async fn carry_out(&mut self, command: Command) {
        match command {
            Command::Deploy {
                app,
                spec,
                id,
                created,
                done,
            } => {
                let outcome = self.deploy(&app, spec, id, created).await;
                let _ = done.send(outcome);
            }
            Command::Rollback { app, done } => {
                let _ = done.send(self.rollback(&app).await);
            }
            Command::Drain { app, id, done } => {
                let _ = done.send(self.drain_release(&app, &id).await);
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
            self.drain(app, &previous).await;
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
        if self.running.contains_key(&(app.to_owned(), id.to_owned())) {
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

        let sockets = host.sockets.clone();
        self.running
            .insert((app.to_owned(), id.to_owned()), Running { host, sockets });
        self.set_state(&release, ReleaseState::Ready, "");
        self.events.emit(
            "release.ready",
            &subject,
            "info",
            &format!(
                "{} workers ready in {:.1} s",
                spec.processes,
                started.elapsed().as_secs_f64()
            ),
        );
        Ok(())
    }

    /// Point the edge at a release's workers.
    async fn switch_to(&mut self, app: &str, id: &str) {
        let subject = format!("{app}@{id}");
        let Some(running) = self.running.get(&(app.to_owned(), id.to_owned())) else {
            return;
        };

        match muru_edge::link::LinkPool::connect(&running.sockets).await {
            Ok(pool) => {
                self.pools.replace(pool);
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
                    &format!("the edge could not connect to the workers: {error:#}"),
                );
            }
        }
    }

    /// Section 9.5: with traffic already elsewhere, wait for the release's
    /// requests in flight to finish, then stop its host.
    async fn drain(&mut self, app: &str, id: &str) {
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };
        let Some(running) = self.running.remove(&(app.to_owned(), id.to_owned())) else {
            if release.state.is_running() {
                self.set_state(&release, ReleaseState::Stopped, "");
            }
            return;
        };

        // The previous pool is gone from the edge, so what is in flight on
        // these workers is what the edge had already dispatched; a short
        // wait lets it finish before the host is told to stop.
        self.set_state(
            &release,
            ReleaseState::Draining,
            "requests in flight are finishing",
        );
        self.events
            .emit("release.draining", &subject, "info", "draining");
        tokio::time::sleep(Duration::from_millis(200)).await;

        self.set_state(&release, ReleaseState::Stopping, "stopping the host");
        let stopped = tokio::time::timeout(DRAIN_TIMEOUT, running.host.stop()).await;
        if stopped.is_err() {
            tracing::warn!(%subject, "the host did not stop within the drain timeout");
        }
        self.set_state(&release, ReleaseState::Stopped, "");
        self.events
            .emit("release.stopped", &subject, "info", "stopped");
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
            self.drain(app, &current).await;
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
            self.pools.replace(muru_edge::link::LinkPool::empty());
            self.store
                .set_app_releases(app, None, Some(id))
                .map_err(|error| error.to_string())?;
            self.events
                .emit("traffic.switched", app, "notice", "no release serves");
        }
        self.drain(app, id).await;

        self.store
            .release(app, id)
            .map_err(|error| error.to_string())
    }

    /// A host that exited on its own fails its release; the release that
    /// serves is started again once.
    async fn check_hosts(&mut self) {
        let mut exited = Vec::new();
        for (key, running) in &mut self.running {
            if let Ok(Some(status)) = running.host.try_exited() {
                exited.push((key.clone(), status));
            }
        }

        for ((app, id), status) in exited {
            self.running.remove(&(app.clone(), id.clone()));
            let subject = format!("{app}@{id}");
            let message = format!("the host exited with {status}");
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
                self.pools.replace(muru_edge::link::LinkPool::empty());
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

    /// Stop every host, for shutdown.
    async fn stop_all(&mut self) {
        self.pools.replace(muru_edge::link::LinkPool::empty());
        let running = std::mem::take(&mut self.running);
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

    fn set_state(&self, release: &Release, state: ReleaseState, message: &str) {
        if let Err(error) = self
            .store
            .set_release_state(&release.app, &release.id, state, message)
        {
            tracing::error!(%error, "recording a release state failed");
        }
    }
}
