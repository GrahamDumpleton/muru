//! The admin API (design specification section 10.6 and appendix B): the
//! `muru.admin.v1` service over the admin Unix socket, which muructl and
//! the dashboard's gateway speak.

use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use muru_proto::admin::v1 as pb;
use muru_proto::admin::v1::admin_server::{Admin, AdminServer};
use tokio::sync::oneshot;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::config::{self, Action, Change, Plan};
use crate::daemon::{Command, Handle};
use crate::model::{Event, Release, ReleaseSpec, ReleaseState, now_unix_ms};

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
        Ok(Response::new(pb::ListAppsResponse {
            apps: apps.into_iter().map(app_message).collect(),
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
        Ok(Response::new(app_message(app)))
    }

    async fn deploy(
        &self,
        request: Request<pb::DeployRequest>,
    ) -> Result<Response<pb::Release>, Status> {
        let request = request.into_inner();
        let previous = self
            .handle
            .store
            .app(&request.app)
            .ok()
            .and_then(|app| app.current.or(app.previous))
            .and_then(|id| self.handle.store.release(&request.app, &id).ok())
            .map(|release| release.spec);
        let spec = config::resolve_spec(&request, previous.as_ref(), None, None, None)
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

        // The application's current release unless one is named.

        let release = if request.release.is_empty() {
            let app = self.handle.store.app(&request.app).map_err(not_found)?;
            app.current.ok_or_else(|| {
                Status::failed_precondition(format!("{} has no current release", request.app))
            })?
        } else {
            request.release.clone()
        };
        self.handle
            .store
            .release(&request.app, &release)
            .map_err(not_found)?;

        let args: serde_json::Value = if request.args_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&request.args_json)
                .map_err(|error| Status::invalid_argument(format!("args: {error}")))?
        };
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
        let answered = tokio::time::timeout(Duration::from_secs(20), done_rx)
            .await
            .map_err(|_| Status::deadline_exceeded("the host did not answer in 20 seconds"))?
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
                    host: route.host,
                    path: route.path,
                    app: route.app,
                    static_dir: route.static_dir.unwrap_or_default(),
                    fallback_app: route
                        .fallback
                        .and_then(|fallback| fallback.app)
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

        let plan = config::plan(&self.handle.store, &document, request.prune);
        let mut report = report_message(&plan);

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

        let plan = config::plan(&self.handle.store, &document, true);
        Ok(Response::new(report_message(&plan)))
    }

    async fn export_config(
        &self,
        request: Request<pb::ExportConfigRequest>,
    ) -> Result<Response<pb::ExportConfigResponse>, Status> {
        let request = request.into_inner();
        let app = (!request.app.is_empty()).then_some(request.app.as_str());
        let document =
            config::export(&self.handle.store, app, request.all_releases).map_err(not_found)?;
        let text = document.to_toml().map_err(internal)?;

        Ok(Response::new(pb::ExportConfigResponse {
            document: text,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            node: config::hostname(),
            time_unix_ms: now_unix_ms(),
            state_version: self.handle.store.state_version().map_err(internal)?,
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

fn app_message(app: crate::model::App) -> pb::App {
    pb::App {
        name: app.name,
        current_release: app.current.unwrap_or_default(),
        previous_release: app.previous.unwrap_or_default(),
        resource_version: app.resource_version,
        created_unix_ms: app.created_unix_ms,
        user: app.user.unwrap_or_default(),
        group: app.group.unwrap_or_default(),
        umask: app.umask.unwrap_or_default(),
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
