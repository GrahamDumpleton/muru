//! The admin socket of a development run (design specification sections
//! 11.7, 13.1 and 13.8): `muructl run` serves the admin protocol the
//! daemon does, so the commands that look into a release's workers or
//! read the edge's figures work against a run unchanged, and refuses the
//! ones that manage state a run does not have. The socket sits in the
//! user runtime directory beside a file that names the run, which is how
//! a command finds it without a flag.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use muru_controller::host::{HostControl, WorkerReply};
use muru_controller::logs::Logs;
use muru_edge::Pools;
use muru_edge::access::AccessLog;
use muru_edge::metrics::{aggregate, pool_samples, render};
use muru_proto::admin::v1 as pb;
use muru_proto::admin::v1::admin_server::{Admin, AdminServer};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

/// How long a debug operation without a period of its own may take.
const ASK_WAIT: Duration = Duration::from_secs(20);
/// The margin given to the host beyond an operation's own period.
const ASK_MARGIN: Duration = Duration::from_secs(15);

/// The host's control connection as the run and the admin service share
/// it: the connection of the release that serves, the requests waiting
/// for their answers, and the numbers that pair them (section 11.7).
#[derive(Default)]
pub struct HostLink {
    control: Option<HostControl>,
    last_request: u64,
    asked: HashMap<u64, oneshot::Sender<Result<Vec<WorkerReply>, String>>>,
}

/// The link, shared between the run's loop and the service.
pub type SharedHostLink = Arc<Mutex<HostLink>>;

impl HostLink {
    /// A release's host serves: its connection replaces the one before,
    /// whose requests still waiting are answered with why they cannot be.
    pub fn attach(&mut self, control: Option<HostControl>) {
        self.control = control;

        for (_, done) in self.asked.drain() {
            let _ = done.send(Err(
                "the release was replaced before its host answered".to_owned()
            ));
        }
    }

    /// Tell the host the edge routes by the workers it announced.
    pub fn acknowledge(&self, version: u64) {
        if let Some(control) = &self.control {
            control.acknowledge(version);
        }
    }

    /// The host answered a request, or said why no worker was asked.
    pub fn answer(&mut self, id: u64, workers: Vec<WorkerReply>, error: String) {
        if let Some(done) = self.asked.remove(&id) {
            let _ = done.send(if error.is_empty() {
                Ok(workers)
            } else {
                Err(error)
            });
        }
    }

    /// Send a request to the host, returning what the answer arrives on.
    fn ask(
        &mut self,
        op: &str,
        worker: Option<u32>,
        args: &serde_json::Value,
    ) -> Result<oneshot::Receiver<Result<Vec<WorkerReply>, String>>, String> {
        let Some(control) = &self.control else {
            return Err("the run has no control connection to its host".to_owned());
        };

        self.last_request += 1;
        let (done_tx, done_rx) = oneshot::channel();
        control.request(self.last_request, op, worker, args);
        self.asked.insert(self.last_request, done_tx);
        Ok(done_rx)
    }
}

/// What the service answers from: the run's facts, the edge's counters
/// and logs, and the host's control connection.
pub struct RunState {
    /// The application, as the run's log names it.
    pub app: String,
    /// The release that serves, by number.
    pub release: Arc<AtomicU32>,
    /// The project directory.
    pub project: PathBuf,
    /// The run directory the hosts live under.
    pub run_dir: PathBuf,
    /// The admin socket.
    pub socket: PathBuf,
    /// The addresses the edge serves, as URLs.
    pub listeners: Vec<String>,
    /// Where the dashboard is served, when it is.
    pub dashboard: Option<SocketAddr>,
    /// When the run started, Unix milliseconds.
    pub started_unix_ms: u64,
    /// The worker pools.
    pub pools: Arc<Pools>,
    /// The listeners' connection counts.
    pub connections: muru_edge::ConnectionCounts,
    /// The access log.
    pub access_log: Arc<AccessLog>,
    /// Recent host and worker output.
    pub logs: Logs,
    /// The host's control connection.
    pub host: SharedHostLink,
}

impl RunState {
    /// The release that serves, as the log names it.
    pub fn release_id(&self) -> String {
        format!("r{}", self.release.load(Ordering::Relaxed))
    }

    /// Check that a target names this run: its application, or none, and
    /// the release that serves, or none.
    pub fn target(&self, app: &str, release: &str) -> Result<String, String> {
        if !app.is_empty() && app != self.app {
            return Err(format!(
                "no application {app} here: this is the development run of {}",
                self.app
            ));
        }

        let serving = self.release_id();

        if !release.is_empty() && release != serving {
            return Err(format!(
                "{}@{release} is not serving: the development run is at release {serving}",
                self.app
            ));
        }

        Ok(serving)
    }

    /// Ask the host something of the workers and wait for the answers,
    /// as the daemon does for `muructl debug` (section 11.7). What the
    /// daemon records as an event, the run prints.
    pub async fn ask(
        &self,
        op: &str,
        worker: Option<u32>,
        args: serde_json::Value,
    ) -> Result<Vec<WorkerReply>, String> {
        let wait = args
            .get("timeout_s")
            .and_then(serde_json::Value::as_f64)
            .map_or(ASK_WAIT, |seconds| {
                Duration::from_secs_f64(seconds.clamp(0.0, 3600.0)) + ASK_MARGIN
            });
        let whom = worker.map_or_else(
            || "every worker".to_owned(),
            |index| format!("worker {index}"),
        );

        match op {
            "attach" => println!("muru: debug.attached: a debugger session opened in {whom}"),
            "exec" => {
                let code: String = args
                    .get("code")
                    .and_then(|code| code.as_str())
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
                println!("muru: debug.exec: ran code in {whom}: {code}");
            }
            "profile" | "trace" | "memory" => {
                let seconds = args
                    .get("duration_s")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0);
                println!("muru: debug.{op}: {op} of {whom} for {seconds} s");
            }
            _ => {}
        }

        ask(&self.host, op, worker, &args, wait).await
    }
}

/// Ask the host something of the workers through the shared link and
/// wait for the answers.
pub async fn ask(
    host: &SharedHostLink,
    op: &str,
    worker: Option<u32>,
    args: &serde_json::Value,
    wait: Duration,
) -> Result<Vec<WorkerReply>, String> {
    let answer = host
        .lock()
        .expect("host link poisoned")
        .ask(op, worker, args)?;
    tokio::time::timeout(wait, answer)
        .await
        .map_err(|_| format!("the host did not answer in {} seconds", wait.as_secs()))?
        .map_err(|_| "the run is stopping".to_owned())?
}

/// What the dashboard's worker pages ask of a run's workers (section
/// 10.7), answered through the shared link as `muructl debug` is.
pub fn debug_source(
    host: SharedHostLink,
    app: String,
    release: Arc<AtomicU32>,
) -> muru_dashboard::DebugSource {
    Arc::new(move |asked: muru_dashboard::DebugAsk| {
        let host = Arc::clone(&host);
        let app = app.clone();
        let release = Arc::clone(&release);
        Box::pin(async move {
            let serving = format!("r{}", release.load(Ordering::Relaxed));

            if asked.app != app || asked.release != serving {
                return Err(format!(
                    "{}@{} is not serving: this run is at {app}@{serving}",
                    asked.app, asked.release
                ));
            }

            let args = if asked.op == "memory" {
                serde_json::json!({
                    "trace": false,
                    "snapshot": "",
                    "duration_s": 0.0,
                    "top": 20,
                    "timeout_s": 30.0,
                })
            } else {
                serde_json::json!({})
            };
            let wait = args
                .get("timeout_s")
                .and_then(serde_json::Value::as_f64)
                .map_or(ASK_WAIT, |seconds| {
                    Duration::from_secs_f64(seconds) + ASK_MARGIN
                });
            let replies = ask(&host, &asked.op, asked.worker, &args, wait).await?;
            Ok(muru_dashboard::DebugAnswer {
                release: asked.release,
                workers: replies
                    .into_iter()
                    .map(|reply| muru_dashboard::WorkerAnswer {
                        index: reply.index,
                        pid: reply.pid,
                        ok: reply.ok,
                        error: (!reply.error.is_empty()).then_some(reply.error),
                        result: serde_json::from_str(&reply.result_json).ok(),
                    })
                    .collect(),
            })
        })
    })
}

/// The Prometheus text of a run: the edge's request metrics, the pools'
/// and the listeners', which `/metrics` on the dashboard shows too.
pub fn metrics_text(
    pools: &Pools,
    access_log: &AccessLog,
    connections: &muru_edge::ConnectionCounts,
) -> String {
    let routing = pools.current();
    let samples = access_log
        .metrics()
        .samples()
        .into_iter()
        .chain(pool_samples(&routing))
        .chain(connections.samples());
    render(&aggregate(samples))
}

/// What a run writes beside its socket, so a command can find it and
/// say which run it found (section 13.8).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunInfo {
    /// The run's pid.
    pub pid: u32,
    /// The application.
    pub app: String,
    /// The project directory.
    pub project: PathBuf,
    /// The address the application is served on.
    pub listen: String,
    /// The admin socket.
    pub socket: PathBuf,
}

/// The run's place in the user runtime directory, removed when dropped.
pub struct Registration {
    directory: PathBuf,
    /// The admin socket inside it.
    pub socket: PathBuf,
}

impl Registration {
    /// Make the run's directory under `runs` in the runtime directory,
    /// named by the port the application is served on so two runs never
    /// share one, and write the file that names the run.
    pub fn new(
        runtime: &Path,
        listen: SocketAddr,
        app: &str,
        project: &Path,
    ) -> anyhow::Result<Self> {
        let runs = runtime.join("runs");
        std::fs::create_dir_all(&runs).with_context(|| format!("creating {}", runs.display()))?;
        let directory = runs.join(listen.port().to_string());
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory)
            .with_context(|| format!("creating {}", directory.display()))?;

        // The socket admits whoever may enter the directory, which is its
        // owner alone, as the daemon's runtime directory is.

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }

        let socket = directory.join("admin.sock");
        let info = RunInfo {
            pid: std::process::id(),
            app: app.to_owned(),
            project: project.to_path_buf(),
            listen: listen.to_string(),
            socket: socket.clone(),
        };
        std::fs::write(directory.join("run.json"), serde_json::to_vec(&info)?)?;
        Ok(Self { directory, socket })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// The development runs listening now, from the runtime directory; the
/// leavings of a run that is gone are removed on the way.
pub fn runs(runtime: &Path) -> Vec<RunInfo> {
    let Ok(entries) = std::fs::read_dir(runtime.join("runs")) else {
        return Vec::new();
    };
    let mut found = Vec::new();

    for entry in entries.flatten() {
        let info = std::fs::read(entry.path().join("run.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RunInfo>(&bytes).ok());

        match info {
            Some(info) if muru_controller::host::alive(info.pid) && info.socket.exists() => {
                found.push(info);
            }
            _ => {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    found.sort_by(|a, b| a.listen.cmp(&b.listen));
    found
}

/// Serve the admin protocol on the run's socket until told to stop.
pub async fn serve(
    state: Arc<RunState>,
    shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<impl std::future::Future<Output = anyhow::Result<()>>> {
    let socket = state.socket.clone();
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket).map_err(|error| {
        anyhow::anyhow!(
            "cannot listen on the admin socket {}: {error}; a shorter MURU_RUN_DIR may be \
             needed, since socket paths are limited to about a hundred bytes",
            socket.display()
        )
    })?;
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
    let service = AdminServer::new(Service { state });

    Ok(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming_shutdown(incoming, async move {
                let _ = shutdown.await;
            })
            .await
            .map_err(anyhow::Error::from)
    })
}

struct Service {
    state: Arc<RunState>,
}

type EventStream = Pin<Box<dyn Stream<Item = Result<pb::Event, Status>> + Send>>;
type LogStream = Pin<Box<dyn Stream<Item = Result<pb::LogLine, Status>> + Send>>;

impl Service {
    /// Why a command that manages the daemon's state is refused here.
    fn needs_the_daemon(&self, what: &str) -> Status {
        Status::failed_precondition(format!(
            "{what} needs the daemon: this is the development run of {} ({}), which has no \
             releases or configuration to manage (muructl daemon start)",
            self.state.app,
            self.state.listeners.join(", ")
        ))
    }

    /// The lines asked for, then those that follow, as a stream the
    /// client can drop at any time.
    fn lines(
        &self,
        app: &str,
        release: &str,
        first: Vec<String>,
        live: Option<broadcast::Receiver<String>>,
    ) -> LogStream {
        let (tx, rx) = mpsc::channel::<Result<pb::LogLine, Status>>(64);
        let app = app.to_owned();
        let release = release.to_owned();
        tokio::spawn(async move {
            let message = |line: String| {
                Ok(pb::LogLine {
                    app: app.clone(),
                    release: release.clone(),
                    line,
                })
            };

            for line in first {
                if tx.send(message(line)).await.is_err() {
                    return;
                }
            }

            let Some(mut live) = live else {
                return;
            };

            loop {
                match live.recv().await {
                    Ok(line) => {
                        if tx.send(message(line)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

/// The commands a run does not serve, each saying so. The methods are
/// written as `async_trait` desugars them, since the attribute on the
/// impl runs before this macro expands.
macro_rules! needs_the_daemon {
    ($($name:ident($request:ty) -> $response:ty, $what:literal;)*) => {
        $(
            fn $name<'life0, 'async_trait>(
                &'life0 self,
                _: Request<$request>,
            ) -> Pin<Box<dyn std::future::Future<Output = Result<Response<$response>, Status>> + Send + 'async_trait>>
            where
                'life0: 'async_trait,
                Self: 'async_trait,
            {
                Box::pin(async move { Err(self.needs_the_daemon($what)) })
            }
        )*
    };
}

#[tonic::async_trait]
impl Admin for Service {
    type WatchEventsStream = EventStream;
    type TailLogsStream = LogStream;

    async fn get_status(
        &self,
        _: Request<pb::GetStatusRequest>,
    ) -> Result<Response<pb::Status>, Status> {
        let state = &self.state;
        Ok(Response::new(pb::Status {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            state_dir: String::new(),
            run_dir: state.run_dir.display().to_string(),
            socket: state.socket.display().to_string(),
            listeners: state.listeners.clone(),
            started_unix_ms: state.started_unix_ms,
            apps: 1,
            releases: 1,
            edge_pid: std::process::id(),
            metrics: state
                .dashboard
                .map(|address| address.to_string())
                .unwrap_or_default(),
            websockets: state.access_log.websockets().len() as u32,
            dashboard: state
                .dashboard
                .map(|address| address.to_string())
                .unwrap_or_default(),
            budget: String::new(),
            kind: "run".to_owned(),
            app: state.app.clone(),
            release: state.release_id(),
            project: state.project.display().to_string(),
        }))
    }

    async fn tail_logs(
        &self,
        request: Request<pb::TailLogsRequest>,
    ) -> Result<Response<LogStream>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let lines = request.lines as usize;

        // The lines are read after subscribing, so none is missed between
        // the two; the ring and the stream are both in memory, since a
        // run writes no log files (section 13.8).

        if request.access {
            let live = request.follow.then(|| state.access_log.subscribe());
            let first = state.access_log.recent_lines(lines);
            return Ok(Response::new(self.lines("", "", first, live)));
        }

        if request.app.is_empty() {
            return Err(Status::invalid_argument(
                "name an application, or ask for the access log",
            ));
        }

        let release = state
            .target(&request.app, &request.release)
            .map_err(Status::not_found)?;
        let live = request.follow.then(|| state.logs.subscribe());
        let first = state.logs.recent(lines);
        Ok(Response::new(self.lines(&state.app, &release, first, live)))
    }

    async fn reopen_logs(
        &self,
        _: Request<pb::ReopenLogsRequest>,
    ) -> Result<Response<pb::ReopenLogsResponse>, Status> {
        Err(Status::failed_precondition(format!(
            "a development run writes no log files to reopen: the output of {} goes to the \
             terminal",
            self.state.app
        )))
    }

    async fn get_metrics(
        &self,
        _: Request<pb::GetMetricsRequest>,
    ) -> Result<Response<pb::MetricsText>, Status> {
        let state = &self.state;
        Ok(Response::new(pb::MetricsText {
            text: metrics_text(&state.pools, &state.access_log, &state.connections),
        }))
    }

    async fn list_web_sockets(
        &self,
        request: Request<pb::ListWebSocketsRequest>,
    ) -> Result<Response<pb::WebSocketList>, Status> {
        let request = request.into_inner();
        let report = muru_edge::endpoints::report(
            &self.state.access_log,
            &request.app,
            request.limit as usize,
        );
        Ok(Response::new(pb::WebSocketList {
            open: report.open,
            endpoints: report
                .endpoints
                .into_iter()
                .map(|endpoint| pb::WebSocketEndpoint {
                    app: endpoint.app,
                    pattern: endpoint.pattern,
                    open: endpoint.open,
                })
                .collect(),
            sockets: report
                .sockets
                .into_iter()
                .map(|socket| pb::OpenWebSocket {
                    request_id: socket.request_id,
                    client: socket.client,
                    app: socket.app,
                    release: socket.release,
                    host: socket.host,
                    group: socket.group,
                    mount: socket.mount,
                    path: socket.path,
                    worker: socket.worker,
                    opened_unix_ms: socket.opened_unix_ms,
                    received: socket.received,
                    sent: socket.sent,
                    received_bytes: socket.received_bytes,
                    sent_bytes: socket.sent_bytes,
                })
                .collect(),
        }))
    }

    async fn debug(
        &self,
        request: Request<pb::DebugRequest>,
    ) -> Result<Response<pb::DebugResponse>, Status> {
        let request = request.into_inner();
        let release = self
            .state
            .target(&request.app, &request.release)
            .map_err(Status::not_found)?;

        let args: serde_json::Value = if request.args_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&request.args_json)
                .map_err(|error| Status::invalid_argument(format!("args: {error}")))?
        };

        // Section 13.1: exec and a debugger session need no debug_exec
        // here, since the person at the terminal started the workers.

        let answered = self
            .state
            .ask(&request.op, request.worker, args)
            .await
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(pb::DebugResponse {
            workers: answered
                .into_iter()
                .map(|reply| pb::WorkerDebug {
                    index: reply.index,
                    pid: reply.pid,
                    ok: reply.ok,
                    error: reply.error,
                    result_json: reply.result_json,
                })
                .collect(),
            release,
        }))
    }

    async fn list_events(
        &self,
        _: Request<pb::ListEventsRequest>,
    ) -> Result<Response<pb::ListEventsResponse>, Status> {
        Err(self.events_are_printed())
    }

    async fn watch_events(
        &self,
        _: Request<pb::WatchEventsRequest>,
    ) -> Result<Response<EventStream>, Status> {
        Err(self.events_are_printed())
    }

    needs_the_daemon! {
        list_apps(pb::ListAppsRequest) -> pb::ListAppsResponse, "muructl app";
        get_app(pb::GetAppRequest) -> pb::App, "muructl app";
        deploy(pb::DeployRequest) -> pb::Release, "muructl deploy";
        list_releases(pb::ListReleasesRequest) -> pb::ListReleasesResponse, "muructl release";
        get_release(pb::GetReleaseRequest) -> pb::Release, "muructl release";
        wait_release(pb::WaitReleaseRequest) -> pb::Release, "muructl release wait";
        rollback(pb::RollbackRequest) -> pb::Release, "muructl rollback";
        drain_release(pb::DrainReleaseRequest) -> pb::Release, "muructl drain";
        start_rollout(pb::StartRolloutRequest) -> pb::Rollout, "muructl rollout";
        get_rollout(pb::GetRolloutRequest) -> pb::Rollout, "muructl rollout";
        list_rollouts(pb::ListRolloutsRequest) -> pb::ListRolloutsResponse, "muructl rollout";
        pause_rollout(pb::RolloutControlRequest) -> pb::Rollout, "muructl rollout";
        resume_rollout(pb::RolloutControlRequest) -> pb::Rollout, "muructl rollout";
        promote_rollout(pb::RolloutControlRequest) -> pb::Rollout, "muructl rollout";
        abort_rollout(pb::RolloutControlRequest) -> pb::Rollout, "muructl rollout";
        promote_release(pb::PromoteReleaseRequest) -> pb::Release, "muructl rollout";
        set_scale(pb::SetScaleRequest) -> pb::ScaleStatus, "muructl scale";
        get_scale_status(pb::GetScaleStatusRequest) -> pb::ScaleStatus, "muructl scale";
        set_maintenance(pb::SetMaintenanceRequest) -> pb::MaintenanceStatus, "muructl maintenance";
        dashboard_login(pb::DashboardLoginRequest) -> pb::DashboardLoginLink, "muructl dashboard";
        shutdown(pb::ShutdownRequest) -> pb::ShutdownResponse, "muructl daemon stop";
        list_hosts(pb::ListHostsRequest) -> pb::ListHostsResponse, "muructl host";
        list_routes(pb::ListRoutesRequest) -> pb::ListRoutesResponse, "muructl route";
        list_upstreams(pb::ListUpstreamsRequest) -> pb::ListUpstreamsResponse, "muructl upstream";
        apply_config(pb::ApplyConfigRequest) -> pb::ConfigReport, "muructl apply";
        diff_config(pb::DiffConfigRequest) -> pb::ConfigReport, "muructl config diff";
        export_config(pb::ExportConfigRequest) -> pb::ExportConfigResponse, "muructl config show";
        reload_config(pb::ReloadConfigRequest) -> pb::ReloadConfigResponse, "muructl daemon reload";
        upgrade_daemon(pb::UpgradeDaemonRequest) -> pb::UpgradeDaemonResponse, "muructl daemon upgrade";
        list_overlays(pb::ListOverlaysRequest) -> pb::ListOverlaysResponse, "muructl overlay";
        control_overlay(pb::ControlOverlayRequest) -> pb::ControlOverlayResponse, "muructl overlay";
        preview_overlay(pb::PreviewOverlayRequest) -> pb::PreviewOverlayResponse, "muructl overlay";
    }
}

impl Service {
    /// Why `muructl events` is refused: a run prints its events.
    fn events_are_printed(&self) -> Status {
        Status::failed_precondition(format!(
            "a development run keeps no event log: the events of {} are printed to its terminal",
            self.state.app
        ))
    }
}
