//! The admin API (design specification section 10.6 and appendix B): the
//! `muru.admin.v1` service over the admin Unix socket, which muructl and
//! the dashboard's gateway speak.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use muru_proto::admin::v1 as pb;
use muru_proto::admin::v1::admin_server::{Admin, AdminServer};
use tokio::sync::oneshot;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::config::{self, Action, Change, Plan};
use crate::daemon::{Command, Handle, RolloutAction};
use crate::model::{
    Event, Release, ReleaseSpec, ReleaseState, Rollout, RolloutGates, RolloutStep, now_unix_ms,
};

/// Serve the API on the socket until the returned future ends.
pub async fn serve(
    socket: PathBuf,
    handle: Handle,
    shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<impl std::future::Future<Output = anyhow::Result<()>>> {
    // Unix socket paths are limited to about a hundred bytes, which a
    // deep runtime directory can exceed (section 13.8 keeps the defaults
    // short for that reason).

    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket).map_err(|error| {
        anyhow::anyhow!(
            "cannot listen on the admin socket {}: {error}; a shorter --run-dir may be needed, \
             since socket paths are limited to about a hundred bytes",
            socket.display()
        )
    })?;
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
    let service = AdminServer::new(Service { handle });

    // The server stops taking connections when told and finishes the
    // responses in flight, so the answer to a Shutdown call is flushed
    // before the daemon exits.

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
    handle: Handle,
}

type EventStream = Pin<Box<dyn Stream<Item = Result<pb::Event, Status>> + Send>>;
type LogStream = Pin<Box<dyn Stream<Item = Result<pb::LogLine, Status>> + Send>>;

#[tonic::async_trait]
impl Admin for Service {
    async fn get_status(
        &self,
        _: Request<pb::GetStatusRequest>,
    ) -> Result<Response<pb::Status>, Status> {
        let info = &self.handle.info;
        let apps = self.handle.store.apps().map_err(internal)?;
        let releases = self.handle.store.releases(None).map_err(internal)?;

        Ok(Response::new(pb::Status {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            state_dir: info.locations.state.path.display().to_string(),
            run_dir: info.locations.run.path.display().to_string(),
            socket: info.locations.admin_socket().display().to_string(),
            listeners: info
                .listeners
                .iter()
                .map(|(address, tls)| {
                    format!("{}://{address}", if *tls { "https" } else { "http" })
                })
                .collect(),
            started_unix_ms: info.started_unix_ms,
            apps: apps.len() as u32,
            releases: releases.len() as u32,
            edge_pid: info.edge_pid.load(std::sync::atomic::Ordering::Relaxed),
            metrics: info
                .metrics
                .map(|address| address.to_string())
                .unwrap_or_default(),
            dashboard: info
                .dashboard
                .map(|address| address.to_string())
                .unwrap_or_default(),
        }))
    }

    async fn list_apps(
        &self,
        _: Request<pb::ListAppsRequest>,
    ) -> Result<Response<pb::ListAppsResponse>, Status> {
        let apps = self.handle.store.apps().map_err(internal)?;
        let maintenance = self.handle.store.maintenance().map_err(internal)?;
        Ok(Response::new(pb::ListAppsResponse {
            apps: apps
                .into_iter()
                .map(|app| {
                    let on = maintenance.iter().any(|(name, _)| *name == app.name);
                    app_message(app, on)
                })
                .collect(),
        }))
    }

    async fn get_app(
        &self,
        request: Request<pb::GetAppRequest>,
    ) -> Result<Response<pb::App>, Status> {
        let app = self
            .handle
            .store
            .app(&request.into_inner().name)
            .map_err(not_found)?;
        let on = self
            .handle
            .store
            .maintenance()
            .map_err(internal)?
            .iter()
            .any(|(name, _)| *name == app.name);
        Ok(Response::new(app_message(app, on)))
    }

    async fn set_maintenance(
        &self,
        request: Request<pb::SetMaintenanceRequest>,
    ) -> Result<Response<pb::MaintenanceStatus>, Status> {
        let request = request.into_inner();
        self.handle.store.app(&request.app).map_err(not_found)?;
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::SetMaintenance {
                app: request.app.clone(),
                on: request.on,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable(STOPPING))?;
        let since = done_rx
            .await
            .map_err(|_| Status::unavailable(STOPPING))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(pb::MaintenanceStatus {
            app: request.app,
            on: since.is_some(),
            since_unix_ms: since.unwrap_or(0),
        }))
    }

    async fn deploy(
        &self,
        request: Request<pb::DeployRequest>,
    ) -> Result<Response<pb::Release>, Status> {
        let request = request.into_inner();
        self.check_freeze(&request.app, "deploy", request.override_freeze)?;
        let previous = self
            .handle
            .store
            .app(&request.app)
            .ok()
            .and_then(|app| app.current.or(app.previous))
            .and_then(|id| self.handle.store.release(&request.app, &id).ok())
            .map(|release| release.spec);
        let spec = config::resolve_spec(&request, previous.as_ref(), &config::Tables::default())
            .map_err(Status::invalid_argument)?;
        let (created_tx, created_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Deploy {
                app: request.app.clone(),
                spec: Box::new(spec),
                id: (!request.release.is_empty()).then(|| request.release.clone()),
                retain: request
                    .retain_ms
                    .map_or(crate::daemon::RETENTION, Duration::from_millis),
                hold: request.hold,
                created: created_tx,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?;

        // Without wait, the record is enough; with it, the outcome.

        let created = created_rx
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?
            .map_err(Status::invalid_argument)?;

        if !request.wait {
            return Ok(Response::new(release_message(created, &self.handle)));
        }

        let release = done_rx
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(release_message(release, &self.handle)))
    }

    async fn list_releases(
        &self,
        request: Request<pb::ListReleasesRequest>,
    ) -> Result<Response<pb::ListReleasesResponse>, Status> {
        let app = request.into_inner().app;
        let app = (!app.is_empty()).then_some(app);
        let releases = self
            .handle
            .store
            .releases(app.as_deref())
            .map_err(internal)?;
        Ok(Response::new(pb::ListReleasesResponse {
            releases: releases
                .into_iter()
                .map(|release| release_message(release, &self.handle))
                .collect(),
        }))
    }

    async fn get_release(
        &self,
        request: Request<pb::GetReleaseRequest>,
    ) -> Result<Response<pb::Release>, Status> {
        let request = request.into_inner();
        let release = self
            .handle
            .store
            .release(&request.app, &request.release)
            .map_err(not_found)?;
        Ok(Response::new(release_message(release, &self.handle)))
    }

    async fn wait_release(
        &self,
        request: Request<pb::WaitReleaseRequest>,
    ) -> Result<Response<pb::Release>, Status> {
        let request = request.into_inner();
        let mut states = Vec::new();

        for name in &request.states {
            states.push(
                name.parse::<ReleaseState>()
                    .map_err(Status::invalid_argument)?,
            );
        }

        if states.is_empty() {
            states = vec![ReleaseState::Ready, ReleaseState::Failed];
        }

        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(if request.timeout_ms == 0 {
                300_000
            } else {
                request.timeout_ms
            });

        loop {
            let release = self
                .handle
                .store
                .release(&request.app, &request.release)
                .map_err(not_found)?;

            if states.contains(&release.state) {
                return Ok(Response::new(release_message(release, &self.handle)));
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(Status::deadline_exceeded(format!(
                    "{}@{} is {} after the timeout",
                    request.app, request.release, release.state
                )));
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn rollback(
        &self,
        request: Request<pb::RollbackRequest>,
    ) -> Result<Response<pb::Release>, Status> {
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Rollback {
                app: request.into_inner().app,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?;
        let release = done_rx
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(release_message(release, &self.handle)))
    }

    async fn drain_release(
        &self,
        request: Request<pb::DrainReleaseRequest>,
    ) -> Result<Response<pb::Release>, Status> {
        let request = request.into_inner();
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Drain {
                app: request.app,
                id: request.release,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?;
        let release = done_rx
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(release_message(release, &self.handle)))
    }

    type TailLogsStream = LogStream;

    async fn tail_logs(
        &self,
        request: Request<pb::TailLogsRequest>,
    ) -> Result<Response<LogStream>, Status> {
        let request = request.into_inner();
        let locations = &self.handle.info.locations;

        // The access log is one file; a release's output is its host's
        // file, the current release's when none is named.

        let (app, release, path) = if request.access {
            (String::new(), String::new(), locations.access_log())
        } else {
            if request.app.is_empty() {
                return Err(Status::invalid_argument(
                    "name an application, or ask for the access log",
                ));
            }

            let app = self.handle.store.app(&request.app).map_err(not_found)?;

            let release = if request.release.is_empty() {
                app.current.clone().ok_or_else(|| {
                    Status::failed_precondition(format!("{} has no current release", app.name))
                })?
            } else {
                self.handle
                    .store
                    .release(&app.name, &request.release)
                    .map_err(not_found)?
                    .id
            };

            let path = locations.release_log(&app.name, &release);
            (app.name, release, path)
        };

        if !request.follow && !path.exists() {
            return Err(Status::not_found(format!(
                "no log file at {}",
                path.display()
            )));
        }

        let lines = crate::logs::tail(path, request.lines as usize, request.follow);
        let stream = tokio_stream::StreamExt::map(lines, move |line| {
            Ok(pb::LogLine {
                app: app.clone(),
                release: release.clone(),
                line,
            })
        });
        Ok(Response::new(Box::pin(stream)))
    }

    async fn reopen_logs(
        &self,
        request: Request<pb::ReopenLogsRequest>,
    ) -> Result<Response<pb::ReopenLogsResponse>, Status> {
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::ReopenLogs {
                workers: request.into_inner().workers,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable(STOPPING))?;
        let reopened = done_rx.await.map_err(|_| Status::unavailable(STOPPING))?;
        Ok(Response::new(pb::ReopenLogsResponse {
            edge: reopened.edge,
            hosts: reopened.hosts,
        }))
    }

    async fn get_metrics(
        &self,
        _: Request<pb::GetMetricsRequest>,
    ) -> Result<Response<pb::MetricsText>, Status> {
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Metrics { done: done_tx })
            .await
            .map_err(|_| Status::unavailable(STOPPING))?;
        let text = done_rx.await.map_err(|_| Status::unavailable(STOPPING))?;
        Ok(Response::new(pb::MetricsText { text }))
    }

    async fn reload_config(
        &self,
        _: Request<pb::ReloadConfigRequest>,
    ) -> Result<Response<pb::ReloadConfigResponse>, Status> {
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Reload { done: done_tx })
            .await
            .map_err(|_| Status::unavailable(STOPPING))?;
        let message = done_rx
            .await
            .map_err(|_| Status::unavailable(STOPPING))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(pb::ReloadConfigResponse { message }))
    }

    async fn list_events(
        &self,
        request: Request<pb::ListEventsRequest>,
    ) -> Result<Response<pb::ListEventsResponse>, Status> {
        let request = request.into_inner();

        let limit = if request.limit == 0 {
            100
        } else {
            request.limit as usize
        };
        let events = self
            .handle
            .store
            .events_after(request.after, limit)
            .map_err(internal)?;
        Ok(Response::new(pb::ListEventsResponse {
            events: events.into_iter().map(event_message).collect(),
        }))
    }

    type WatchEventsStream = EventStream;

    async fn watch_events(
        &self,
        request: Request<pb::WatchEventsRequest>,
    ) -> Result<Response<EventStream>, Status> {
        // Subscribe first, then read the backlog, so nothing between the
        // two is missed; duplicates are filtered by sequence.

        let live = self.handle.events.subscribe();
        let backlog = self
            .handle
            .store
            .events_after(request.into_inner().after, 1000)
            .map_err(internal)?;
        let last = backlog.last().map_or(0, |event| event.sequence);

        let first = tokio_stream::iter(backlog.into_iter().map(|event| Ok(event_message(event))));
        let rest = futures::stream::unfold((live, last), |(mut live, last)| async move {
            loop {
                match live.recv().await {
                    Ok(event) if event.sequence > last => {
                        let next = event.sequence;
                        return Some((Ok(event_message(event)), (live, next)));
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return None,
                }
            }
        });
        let stream: EventStream = Box::pin(tokio_stream::StreamExt::chain(first, rest));
        Ok(Response::new(stream))
    }

    async fn debug(
        &self,
        request: Request<pb::DebugRequest>,
    ) -> Result<Response<pb::DebugResponse>, Status> {
        let request = request.into_inner();
        let release = self.current_release(&request.app, &request.release)?;

        let args: serde_json::Value = if request.args_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&request.args_json)
                .map_err(|error| Status::invalid_argument(format!("args: {error}")))?
        };

        // Section 11.7: an operation over a period has the host wait for
        // it, and this waits for the host.

        let wait = args
            .get("timeout_s")
            .and_then(serde_json::Value::as_f64)
            .map_or(Duration::from_secs(20), |seconds| {
                Duration::from_secs_f64(seconds.clamp(0.0, 3600.0)) + Duration::from_secs(15)
            });
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Debug {
                app: request.app.clone(),
                id: release.clone(),
                op: request.op.clone(),
                worker: request.worker,
                args,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?;
        let answered = tokio::time::timeout(wait, done_rx)
            .await
            .map_err(|_| {
                Status::deadline_exceeded(format!(
                    "the host did not answer in {} seconds",
                    wait.as_secs()
                ))
            })?
            .map_err(|_| Status::unavailable("the daemon is stopping"))?
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

    async fn list_overlays(
        &self,
        _: Request<pb::ListOverlaysRequest>,
    ) -> Result<Response<pb::ListOverlaysResponse>, Status> {
        let store = &self.handle.store;
        let states: std::collections::HashMap<String, crate::model::OverlayState> = store
            .overlay_states()
            .map_err(internal)?
            .into_iter()
            .map(|state| (state.name.clone(), state))
            .collect();
        let now = crate::model::now_unix_ms();
        let overlays = store
            .overlays()
            .map_err(internal)?
            .into_iter()
            .map(|(name, overlay)| {
                let state = states.get(&name);
                let window = crate::overlays::window(&overlay, now);
                pb::OverlayInfo {
                    active: state.is_some_and(|state| state.active),
                    state: state.map(|state| state.source.clone()).unwrap_or_default(),
                    since_unix_ms: state.map_or(0, |state| state.since_unix_ms),
                    until_unix_ms: state.and_then(|state| state.until_unix_ms).unwrap_or(0),
                    trigger: crate::overlays::describe_trigger(&overlay),
                    changes: overlay
                        .routes
                        .keys()
                        .map(|route| format!("route {route}"))
                        .chain(overlay.apps.keys().map(|app| format!("app {app}")))
                        .collect(),
                    starts_unix_ms: window
                        .map(|(start, _)| start)
                        .filter(|start| *start > now)
                        .unwrap_or(0),
                    name,
                }
            })
            .collect();
        Ok(Response::new(pb::ListOverlaysResponse { overlays }))
    }

    async fn control_overlay(
        &self,
        request: Request<pb::ControlOverlayRequest>,
    ) -> Result<Response<pb::ControlOverlayResponse>, Status> {
        let request = request.into_inner();

        let action = match request.action.as_str() {
            "activate" => crate::daemon::OverlayAction::Activate {
                until: (request.until_unix_ms > 0).then_some(request.until_unix_ms),
            },
            "end" => crate::daemon::OverlayAction::End,
            "promote" => crate::daemon::OverlayAction::Promote,
            other => {
                return Err(Status::invalid_argument(format!(
                    "{other:?} is not activate, end or promote"
                )));
            }
        };

        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Overlay {
                name: request.name,
                action,
                external: false,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?;
        let message = done_rx
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(pb::ControlOverlayResponse { message }))
    }

    async fn preview_overlay(
        &self,
        request: Request<pb::PreviewOverlayRequest>,
    ) -> Result<Response<pb::PreviewOverlayResponse>, Status> {
        let name = request.into_inner().name;
        let store = &self.handle.store;
        let overlays = store.overlays().map_err(internal)?;
        let overlay = overlays
            .iter()
            .find(|(declared, _)| *declared == name)
            .map(|(_, overlay)| overlay.clone())
            .ok_or_else(|| Status::not_found(format!("no overlay is named {name}")))?;

        // Against the routes as they stand now, with the other active
        // overlays applied.

        let states = store.overlay_states().map_err(internal)?;
        let others: Vec<(String, muru_config::Overlay)> = overlays
            .iter()
            .filter(|(declared, _)| *declared != name)
            .cloned()
            .collect();
        let active = crate::overlays::ordered(&others, &states);
        let mut routes = store.routes().map_err(internal)?;
        crate::overlays::apply_routes(&mut routes, &active);
        let base: std::collections::BTreeSet<String> = store
            .maintenance()
            .map_err(internal)?
            .into_iter()
            .map(|(app, _)| app)
            .collect();
        let maintenance = crate::overlays::apply_maintenance(&base, &active);
        Ok(Response::new(pb::PreviewOverlayResponse {
            changes: crate::overlays::preview(&overlay, &routes, &maintenance),
        }))
    }

    async fn upgrade_daemon(
        &self,
        _: Request<pb::UpgradeDaemonRequest>,
    ) -> Result<Response<pb::UpgradeDaemonResponse>, Status> {
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Upgrade { done: done_tx })
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?;
        let upgrade = done_rx
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(pb::UpgradeDaemonResponse {
            executable: upgrade.executable.display().to_string(),
            version: upgrade.version,
        }))
    }

    async fn shutdown(
        &self,
        request: Request<pb::ShutdownRequest>,
    ) -> Result<Response<pb::ShutdownResponse>, Status> {
        let drain = request.into_inner().drain;
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Shutdown {
                drain,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable("the daemon is stopping"))?;
        let _ = done_rx.await;
        Ok(Response::new(pb::ShutdownResponse {}))
    }

    async fn start_rollout(
        &self,
        request: Request<pb::StartRolloutRequest>,
    ) -> Result<Response<pb::Rollout>, Status> {
        let request = request.into_inner();
        self.check_freeze(&request.app, "start a rollout", request.override_freeze)?;
        let steps = request
            .steps
            .iter()
            .map(|step| RolloutStep {
                percent: step.percent,
                hold_ms: step.hold_ms,
            })
            .collect();
        let gates = RolloutGates {
            max_error_rate: if request.max_error_rate > 0.0 {
                request.max_error_rate
            } else {
                0.01
            },
            max_p95_ms: (request.max_p95_ms > 0).then_some(request.max_p95_ms),
            min_requests: if request.min_requests > 0 {
                request.min_requests
            } else {
                100
            },
        };
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::StartRollout {
                app: request.app,
                release: request.release,
                steps,
                gates,
                approve_each_step: request.approve_each_step,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable(STOPPING))?;
        let rollout = done_rx
            .await
            .map_err(|_| Status::unavailable(STOPPING))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(rollout_message(&rollout)))
    }

    async fn get_rollout(
        &self,
        request: Request<pb::GetRolloutRequest>,
    ) -> Result<Response<pb::Rollout>, Status> {
        let request = request.into_inner();
        let rollout = self
            .handle
            .store
            .rollout(&request.app, &request.rollout)
            .map_err(not_found)?;
        Ok(Response::new(rollout_message(&rollout)))
    }

    async fn list_rollouts(
        &self,
        request: Request<pb::ListRolloutsRequest>,
    ) -> Result<Response<pb::ListRolloutsResponse>, Status> {
        let request = request.into_inner();
        let rollouts = self
            .handle
            .store
            .rollouts((!request.app.is_empty()).then_some(request.app.as_str()))
            .map_err(internal)?;
        Ok(Response::new(pb::ListRolloutsResponse {
            rollouts: rollouts.iter().map(rollout_message).collect(),
        }))
    }

    async fn pause_rollout(
        &self,
        request: Request<pb::RolloutControlRequest>,
    ) -> Result<Response<pb::Rollout>, Status> {
        self.control_rollout(request.into_inner().app, RolloutAction::Pause)
            .await
    }

    async fn resume_rollout(
        &self,
        request: Request<pb::RolloutControlRequest>,
    ) -> Result<Response<pb::Rollout>, Status> {
        self.control_rollout(request.into_inner().app, RolloutAction::Resume)
            .await
    }

    async fn promote_rollout(
        &self,
        request: Request<pb::RolloutControlRequest>,
    ) -> Result<Response<pb::Rollout>, Status> {
        let request = request.into_inner();
        self.check_freeze(&request.app, "promote", request.override_freeze)?;
        self.control_rollout(request.app, RolloutAction::Promote)
            .await
    }

    async fn abort_rollout(
        &self,
        request: Request<pb::RolloutControlRequest>,
    ) -> Result<Response<pb::Rollout>, Status> {
        self.control_rollout(request.into_inner().app, RolloutAction::Abort)
            .await
    }

    async fn promote_release(
        &self,
        request: Request<pb::PromoteReleaseRequest>,
    ) -> Result<Response<pb::Release>, Status> {
        let request = request.into_inner();
        self.check_freeze(&request.app, "promote", request.override_freeze)?;
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::PromoteRelease {
                app: request.app,
                id: request.release,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable(STOPPING))?;
        let release = done_rx
            .await
            .map_err(|_| Status::unavailable(STOPPING))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(release_message(release, &self.handle)))
    }

    async fn set_scale(
        &self,
        request: Request<pb::SetScaleRequest>,
    ) -> Result<Response<pb::ScaleStatus>, Status> {
        let request = request.into_inner();
        let release = self.current_release(&request.app, &request.release)?;

        let group = if request.group.is_empty() {
            muru_edge::routing::DEFAULT_GROUP.to_owned()
        } else {
            request.group.clone()
        };

        // Section 7.5: a floor until a time is a temporary schedule over
        // the application, whichever release serves.

        if let Some(min_workers) = request.min_workers {
            let (done_tx, done_rx) = oneshot::channel();
            self.handle
                .commands
                .send(Command::ScaleFloor {
                    app: request.app.clone(),
                    group,
                    min_workers,
                    max_workers: request.max_workers,
                    until: request.until_unix_ms,
                    done: done_tx,
                })
                .await
                .map_err(|_| Status::unavailable(STOPPING))?;
            done_rx
                .await
                .map_err(|_| Status::unavailable(STOPPING))?
                .map_err(Status::failed_precondition)?;
            return self
                .get_scale_status(Request::new(pb::GetScaleStatusRequest {
                    app: request.app,
                    release,
                }))
                .await;
        }

        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::SetScale {
                app: request.app.clone(),
                id: release.clone(),
                group,
                processes: request.processes,
                unpin: request.unpin,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable(STOPPING))?;
        let groups = done_rx
            .await
            .map_err(|_| Status::unavailable(STOPPING))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(scale_message(&request.app, &release, groups)))
    }

    async fn get_scale_status(
        &self,
        request: Request<pb::GetScaleStatusRequest>,
    ) -> Result<Response<pb::ScaleStatus>, Status> {
        let request = request.into_inner();
        let release = self.current_release(&request.app, &request.release)?;
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::GetScaleStatus {
                app: request.app.clone(),
                id: release.clone(),
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable(STOPPING))?;
        let groups = done_rx
            .await
            .map_err(|_| Status::unavailable(STOPPING))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(scale_message(&request.app, &release, groups)))
    }

    async fn list_hosts(
        &self,
        _: Request<pb::ListHostsRequest>,
    ) -> Result<Response<pb::ListHostsResponse>, Status> {
        let hosts = self.handle.store.hosts().map_err(internal)?;
        Ok(Response::new(pb::ListHostsResponse {
            hosts: hosts
                .into_iter()
                .map(|(name, host)| pb::Host {
                    name,
                    names: host.names,
                    listeners: host.listeners,
                    tls: host.tls.is_some(),
                    certificate: host.tls.map(|tls| tls.certificate).unwrap_or_default(),
                    canonical: host.canonical.unwrap_or_default(),
                    https_redirect: host.https_redirect,
                    use_canonical_name: host.use_canonical_name,
                    default: host.default,
                })
                .collect(),
        }))
    }

    async fn list_upstreams(
        &self,
        _: Request<pb::ListUpstreamsRequest>,
    ) -> Result<Response<pb::ListUpstreamsResponse>, Status> {
        let upstreams = self.handle.store.upstreams().map_err(internal)?;
        Ok(Response::new(pb::ListUpstreamsResponse {
            upstreams: upstreams
                .into_iter()
                .map(|(name, upstream)| pb::Upstream {
                    name,
                    addresses: upstream.addresses,
                    protocol: upstream.protocol.unwrap_or_else(|| "http1".to_owned()),
                    tls: upstream.tls.is_some(),
                    health_path: upstream
                        .health
                        .and_then(|health| health.path)
                        .unwrap_or_default(),
                    trust_muru_headers: upstream.trust_muru_headers,
                })
                .collect(),
        }))
    }

    async fn list_routes(
        &self,
        _: Request<pb::ListRoutesRequest>,
    ) -> Result<Response<pb::ListRoutesResponse>, Status> {
        let routes = self.handle.store.routes().map_err(internal)?;
        Ok(Response::new(pb::ListRoutesResponse {
            routes: routes
                .into_iter()
                .map(|route| pb::Route {
                    traffic: route
                        .traffic
                        .as_ref()
                        .map(|traffic| {
                            traffic
                                .split
                                .iter()
                                .map(|(release, percent)| pb::TrafficShare {
                                    release: release.clone(),
                                    percent: *percent,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    sticky: route
                        .traffic
                        .as_ref()
                        .and_then(|traffic| traffic.sticky)
                        .unwrap_or(false),
                    hash_key: route
                        .traffic
                        .as_ref()
                        .and_then(|traffic| traffic.hash_key.clone())
                        .unwrap_or_default(),
                    internal: route.internal == Some(true),
                    accept_redirects_from: route.accept_redirects_from,
                    auth: route
                        .auth
                        .as_ref()
                        .map(|auth| format!("{} {}", config::auth_handler(auth), auth.path))
                        .unwrap_or_default(),
                    upload: match (&route.upload, route.precheck) {
                        (Some(mode), Some(true)) => format!("{mode}, precheck"),
                        (Some(mode), _) => mode.clone(),
                        (None, _) => String::new(),
                    },
                    max_body_size: route.max_body_size.clone().unwrap_or_default(),
                    host: route.host,
                    path: route.path,
                    app: route.app,
                    upstream: route.upstream,
                    static_dir: route.static_dir.unwrap_or_default(),
                    fallback_app: route
                        .fallback
                        .as_ref()
                        .and_then(|fallback| fallback.app.clone())
                        .unwrap_or_default(),
                    fallback_upstream: route
                        .fallback
                        .and_then(|fallback| fallback.upstream)
                        .unwrap_or_default(),
                })
                .collect(),
        }))
    }

    async fn apply_config(
        &self,
        request: Request<pb::ApplyConfigRequest>,
    ) -> Result<Response<pb::ConfigReport>, Status> {
        let request = request.into_inner();

        let document = match muru_config::Document::parse(&request.document) {
            Ok(document) => document,
            Err(error) => return Ok(Response::new(parse_failure(error))),
        };

        let rate_limits = self
            .handle
            .info
            .rate_limits
            .read()
            .expect("rate limits poisoned")
            .clone();
        let mut plan = config::plan(&self.handle.store, &document, request.prune, &rate_limits);
        let frozen = self.frozen().map_err(internal)?;
        let mut overridden = BTreeMap::new();

        for change in &plan.changes {
            if let Action::Deploy { app, id, .. } = &change.action
                && let Some(by) = frozen.get(app)
            {
                if request.override_freeze {
                    overridden.insert(app.clone(), (id.clone(), by.join(", ")));
                } else {
                    plan.errors.push(format!(
                        "{app}@{id}: deploys of {app} are frozen by overlay {}; give --override-freeze to deploy anyway",
                        by.join(", ")
                    ));
                }
            }
        }

        let mut report = report_message(&plan);

        if !request.dry_run && plan.errors.is_empty() {
            for (app, (id, by)) in &overridden {
                self.handle.events.emit(
                    "deploy.freeze_overridden",
                    app,
                    "warning",
                    &format!("apply deploys {app}@{id} despite the freeze set by overlay {by}"),
                );
            }
        }

        if !plan.errors.is_empty() || request.dry_run {
            return Ok(Response::new(report));
        }

        // The plan is carried out in order, each change's outcome going
        // into the report; a failure does not stop the rest.

        let mut failures = 0;

        for (change, message) in plan.changes.iter().zip(report.changes.iter_mut()) {
            message.outcome = self.carry_out(change).await;

            if message.outcome.starts_with("failed") {
                failures += 1;
            }
        }

        report.applied = true;

        // Hosts and routes reach the edge with the next snapshot.

        if plan
            .changes
            .iter()
            .any(|change| change.action.reconfigures())
            && let Err(error) = self.reconfigure().await
        {
            report
                .warnings
                .push(format!("the edge did not take the new snapshot: {error}"));
            failures += 1;
        }

        let summary = format!(
            "{} changes applied, {} failed, {} releases unchanged",
            plan.changes.len(),
            failures,
            plan.unchanged
        );
        self.handle.events.emit(
            "config.applied",
            "config",
            if failures == 0 { "notice" } else { "warning" },
            &summary,
        );
        Ok(Response::new(report))
    }

    async fn diff_config(
        &self,
        request: Request<pb::DiffConfigRequest>,
    ) -> Result<Response<pb::ConfigReport>, Status> {
        let document = match muru_config::Document::parse(&request.into_inner().document) {
            Ok(document) => document,
            Err(error) => return Ok(Response::new(parse_failure(error))),
        };

        // A diff shows both directions: what the document would add and
        // what desired state holds beyond it.

        let rate_limits = self
            .handle
            .info
            .rate_limits
            .read()
            .expect("rate limits poisoned")
            .clone();
        let plan = config::plan(&self.handle.store, &document, true, &rate_limits);
        Ok(Response::new(report_message(&plan)))
    }

    async fn export_config(
        &self,
        request: Request<pb::ExportConfigRequest>,
    ) -> Result<Response<pb::ExportConfigResponse>, Status> {
        let request = request.into_inner();
        let app = (!request.app.is_empty()).then_some(request.app.as_str());
        let mut document =
            config::export(&self.handle.store, app, request.all_releases).map_err(not_found)?;

        let attributions = if request.effective {
            config::make_effective(&self.handle.store, &mut document, app).map_err(internal)?
        } else {
            Vec::new()
        };

        let text = document.to_toml().map_err(internal)?;

        Ok(Response::new(pb::ExportConfigResponse {
            document: text,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            node: config::hostname(),
            time_unix_ms: now_unix_ms(),
            state_version: self.handle.store.state_version().map_err(internal)?,
            attributions,
        }))
    }
}

impl Service {
    /// Carry out one change of a plan and say how it went: an outcome
    /// starting with "failed" is counted as a failure.
    async fn carry_out(&self, change: &Change) -> String {
        let store = &self.handle.store;

        let outcome = match &change.action {
            Action::CreateApp { app } => store
                .ensure_app(app)
                .map(|_| "created".to_owned())
                .map_err(|error| error.to_string()),
            Action::SetAppIdentity {
                app,
                user,
                group,
                umask,
            } => store
                .set_app_identity(app, user.as_deref(), group.as_deref(), umask.as_deref())
                .map(|()| "set".to_owned())
                .map_err(|error| error.to_string()),
            Action::Deploy { app, id, spec } => self.deploy_declared(app, id, spec).await,
            Action::RemoveRelease { app, id } => self.remove_release(app, id).await,
            Action::RemoveApp { app } => store
                .delete_app(app)
                .map(|()| "removed".to_owned())
                .map_err(|error| error.to_string()),
            Action::PutHost { name, host, new } => store
                .put_host(name, host)
                .map(|()| if *new { "created" } else { "updated" }.to_owned())
                .map_err(|error| error.to_string()),
            Action::RemoveHost { name } => store
                .delete_host(name)
                .map(|()| "removed".to_owned())
                .map_err(|error| error.to_string()),
            Action::PutRoute { route, new } => {
                let before = store
                    .routes()
                    .unwrap_or_default()
                    .into_iter()
                    .find(|stored| stored.host == route.host && stored.path == route.path)
                    .and_then(|stored| stored.traffic);

                // Section 9.4: a changed policy is an event of its own.

                if before != route.traffic {
                    self.handle.events.emit(
                        "traffic_policy.changed",
                        &format!("route {} {}", route.host, route.path),
                        "notice",
                        &crate::config::describe_traffic(route.traffic.as_ref()),
                    );
                }

                store
                    .put_route(route)
                    .map(|()| if *new { "created" } else { "updated" }.to_owned())
                    .map_err(|error| error.to_string())
            }
            Action::RemoveRoute { host, path } => store
                .delete_route(host, path)
                .map(|()| "removed".to_owned())
                .map_err(|error| error.to_string()),
            Action::PutUpstream {
                name,
                upstream,
                new,
            } => store
                .put_upstream(name, upstream)
                .map(|()| if *new { "created" } else { "updated" }.to_owned())
                .map_err(|error| error.to_string()),
            Action::RemoveUpstream { name } => store
                .delete_upstream(name)
                .map(|()| "removed".to_owned())
                .map_err(|error| error.to_string()),
            Action::PutOverlay { name, overlay, new } => store
                .put_overlay(name, overlay)
                .map(|()| if *new { "created" } else { "updated" }.to_owned())
                .map_err(|error| error.to_string()),
            Action::RemoveOverlay { name } => store
                .delete_overlay(name)
                .map(|()| "removed".to_owned())
                .map_err(|error| error.to_string()),
        };

        outcome.unwrap_or_else(|error| format!("failed: {error}"))
    }

    /// Deploy a declared release under its declared id and wait for the
    /// outcome, as a deploy with --wait does.
    async fn deploy_declared(
        &self,
        app: &str,
        id: &str,
        spec: &ReleaseSpec,
    ) -> Result<String, String> {
        let (created_tx, created_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Deploy {
                app: app.to_owned(),
                spec: Box::new(spec.clone()),
                id: Some(id.to_owned()),
                retain: crate::daemon::RETENTION,
                hold: false,
                created: created_tx,
                done: done_tx,
            })
            .await
            .map_err(|_| STOPPING.to_owned())?;

        created_rx.await.map_err(|_| STOPPING.to_owned())??;
        let release = done_rx.await.map_err(|_| STOPPING.to_owned())??;

        let serving = self
            .handle
            .store
            .app(&release.app)
            .ok()
            .and_then(|app| app.current)
            .as_deref()
            == Some(release.id.as_str());
        Ok(format!(
            "{}{}",
            release.state,
            if serving { " and serving" } else { "" }
        ))
    }

    /// Drain a release if it runs, then drop its record.
    async fn remove_release(&self, app: &str, id: &str) -> Result<String, String> {
        let running = self
            .handle
            .store
            .release(app, id)
            .is_ok_and(|release| release.state.is_running());

        let mut outcome = "removed";

        if running {
            let (done_tx, done_rx) = oneshot::channel();
            self.handle
                .commands
                .send(Command::Drain {
                    app: app.to_owned(),
                    id: id.to_owned(),
                    done: done_tx,
                })
                .await
                .map_err(|_| STOPPING.to_owned())?;
            done_rx.await.map_err(|_| STOPPING.to_owned())??;
            outcome = "stopped and removed";
        }

        self.handle
            .store
            .delete_release(app, id)
            .map_err(|error| error.to_string())?;
        Ok(outcome.to_owned())
    }

    /// Push a snapshot with the hosts and routes as they now are.
    async fn reconfigure(&self) -> Result<(), String> {
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Reconfigure { done: done_tx })
            .await
            .map_err(|_| STOPPING.to_owned())?;

        done_rx.await.map_err(|_| STOPPING.to_owned())?
    }
}

/// The outcome when the daemon's command loop has gone.
const STOPPING: &str = "the daemon is stopping";

/// A report with one parse error and nothing else.
fn parse_failure(error: String) -> pb::ConfigReport {
    pb::ConfigReport {
        errors: vec![error],
        ..Default::default()
    }
}

fn report_message(plan: &Plan) -> pb::ConfigReport {
    pb::ConfigReport {
        errors: plan.errors.clone(),
        warnings: plan.warnings.clone(),
        changes: plan
            .changes
            .iter()
            .map(|change| pb::ConfigChange {
                action: change.action.name().to_owned(),
                subject: change.subject(),
                detail: change.detail.clone(),
                outcome: String::new(),
            })
            .collect(),
        unchanged: plan.unchanged,
        applied: false,
    }
}

fn app_message(app: crate::model::App, maintenance: bool) -> pb::App {
    pb::App {
        name: app.name,
        current_release: app.current.unwrap_or_default(),
        previous_release: app.previous.unwrap_or_default(),
        resource_version: app.resource_version,
        created_unix_ms: app.created_unix_ms,
        user: app.user.unwrap_or_default(),
        group: app.group.unwrap_or_default(),
        umask: app.umask.unwrap_or_default(),
        maintenance,
    }
}

impl Service {
    /// The release named, or the application's current release when none
    /// is; checked to exist.
    /// Section 9.6: the applications whose deploys and rollouts an active
    /// overlay freezes, with the overlays that do.
    fn frozen(&self) -> Result<BTreeMap<String, Vec<String>>, crate::store::StoreError> {
        let overlays = self.handle.store.overlays()?;
        let states = self.handle.store.overlay_states()?;
        Ok(crate::overlays::freezes(&crate::overlays::ordered(
            &overlays, &states,
        )))
    }

    /// Refuse to `what` an application an active overlay freezes, unless
    /// overridden, which is recorded as a warning event.
    fn check_freeze(&self, app: &str, what: &str, override_freeze: bool) -> Result<(), Status> {
        let frozen = self.frozen().map_err(internal)?;

        let Some(by) = frozen.get(app) else {
            return Ok(());
        };

        let by = by.join(", ");

        if !override_freeze {
            return Err(Status::failed_precondition(format!(
                "deploys of {app} are frozen by overlay {by}; give --override-freeze to {what} anyway"
            )));
        }

        self.handle.events.emit(
            "deploy.freeze_overridden",
            app,
            "warning",
            &format!("{what} despite the freeze set by overlay {by}"),
        );
        Ok(())
    }

    fn current_release(&self, app: &str, release: &str) -> Result<String, Status> {
        let release = if release.is_empty() {
            let record = self.handle.store.app(app).map_err(not_found)?;
            record.current.ok_or_else(|| {
                Status::failed_precondition(format!("{app} has no current release"))
            })?
        } else {
            release.to_owned()
        };
        self.handle
            .store
            .release(app, &release)
            .map_err(not_found)?;
        Ok(release)
    }

    /// Send a rollout action to the daemon and answer with the rollout.
    async fn control_rollout(
        &self,
        app: String,
        action: RolloutAction,
    ) -> Result<Response<pb::Rollout>, Status> {
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::ControlRollout {
                app,
                action,
                done: done_tx,
            })
            .await
            .map_err(|_| Status::unavailable(STOPPING))?;
        let rollout = done_rx
            .await
            .map_err(|_| Status::unavailable(STOPPING))?
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(rollout_message(&rollout)))
    }
}

/// The scaling state of a release's groups as the API shows it.
fn scale_message(
    app: &str,
    release: &str,
    groups: Vec<(String, crate::scaling::ScaleStatus)>,
) -> pb::ScaleStatus {
    pb::ScaleStatus {
        app: app.to_owned(),
        release: release.to_owned(),
        groups: groups
            .into_iter()
            .map(|(group, status)| pb::GroupScale {
                group,
                mode: status.spec.mode.as_str().to_owned(),
                min_workers: status.spec.min_workers,
                max_workers: status.spec.max_workers,
                desired: status.desired,
                ready: status.ready,
                pinned: status.pinned.is_some(),
                advisory: status.spec.advisory,
                slots_busy: status.busy,
                slots_total: status.total,
                idle_workers: status.idle,
                queue_wait_p95_ms: status.queue_wait_p95_ms,
                last_decision: status
                    .last
                    .as_ref()
                    .map(|(_, what)| what.clone())
                    .unwrap_or_default(),
                last_decision_age_ms: status
                    .last
                    .as_ref()
                    .map(|(age, _)| age.as_millis() as u64)
                    .unwrap_or_default(),
                reaps_last_hour: status.reaps,
                churn_budget: status.spec.churn_budget,
                suspended: status.suspended,
                policy: status.spec.describe(),
                scale_outs: status.decisions.0,
                scale_ins: status.decisions.1,
                floor: status.floor,
                ceiling: status.ceiling,
                capacity_from: status
                    .capacity
                    .as_ref()
                    .map(|capacity| capacity.sources.clone())
                    .unwrap_or_default(),
                scale_in_held: status
                    .capacity
                    .as_ref()
                    .is_some_and(|capacity| capacity.hold),
            })
            .collect(),
    }
}

/// A rollout record as the API shows it.
fn rollout_message(rollout: &Rollout) -> pb::Rollout {
    pb::Rollout {
        app: rollout.app.clone(),
        id: rollout.id.clone(),
        candidate: rollout.candidate.clone(),
        baseline: rollout.baseline.clone(),
        steps: rollout
            .steps
            .iter()
            .map(|step| pb::RolloutStep {
                percent: step.percent,
                hold_ms: step.hold_ms,
            })
            .collect(),
        state: rollout.state.as_str().to_owned(),
        step: rollout.step as u32,
        percent: rollout.percent(),
        message: rollout.message.clone(),
        created_unix_ms: rollout.created_unix_ms,
        updated_unix_ms: rollout.updated_unix_ms,
        max_error_rate: rollout.gates.max_error_rate,
        max_p95_ms: rollout.gates.max_p95_ms.unwrap_or(0),
        min_requests: rollout.gates.min_requests,
        approve_each_step: rollout.approve_each_step,
    }
}

fn release_message(release: Release, handle: &Handle) -> pb::Release {
    let serving = handle
        .store
        .app(&release.app)
        .ok()
        .and_then(|app| app.current)
        .as_deref()
        == Some(release.id.as_str());

    pb::Release {
        app: release.app,
        id: release.id,
        state: release.state.as_str().to_owned(),
        message: release.message,
        source: release.spec.source.display().to_string(),
        entrypoint: release.spec.entrypoint,
        interface: release.spec.interface.as_str().to_owned(),
        python: release.spec.python.display().to_string(),
        processes: release.spec.processes,
        threads: release.spec.threads,
        sync_threads: release.spec.sync_threads,
        concurrency: release.spec.concurrency,
        serving,
        created_unix_ms: release.created_unix_ms,
        resource_version: release.resource_version,
        workers: Vec::new(),
        retained_until_unix_ms: 0,
        kind: if release.spec.http.is_some() {
            "http".to_owned()
        } else {
            "workers".to_owned()
        },
        profile: release
            .spec
            .http
            .as_ref()
            .map(|http| http.profile.as_str().to_owned())
            .unwrap_or_default(),
        instances: release.spec.http.as_ref().map_or(0, |http| http.instances),
        command: release
            .spec
            .http
            .as_ref()
            .map(|http| http.command.clone())
            .unwrap_or_default(),
    }
}

fn event_message(event: Event) -> pb::Event {
    pb::Event {
        sequence: event.sequence,
        time_unix_ms: event.time_unix_ms,
        r#type: event.kind,
        subject: event.subject,
        level: event.level.to_owned(),
        message: event.message,
    }
}

fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(error.to_string())
}

fn not_found(error: impl std::fmt::Display) -> Status {
    Status::not_found(error.to_string())
}
