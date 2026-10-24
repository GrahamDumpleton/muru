//! The admin API (design specification section 10.6 and appendix B): the
//! `muru.admin.v1` service over the admin Unix socket, which muructl and
//! the dashboard's gateway speak.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use muru_proto::admin::v1 as pb;
use muru_proto::admin::v1::admin_server::{Admin, AdminServer};
use tokio::sync::oneshot;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::daemon::{Command, Handle};
use crate::model::{Event, Release, ReleaseSpec, ReleaseState};

/// Serve the API on the socket until the returned future ends.
pub async fn serve(
    socket: PathBuf,
    handle: Handle,
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

    Ok(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
            .map_err(anyhow::Error::from)
    })
}

struct Service {
    handle: Handle,
}

type EventStream = Pin<Box<dyn Stream<Item = Result<pb::Event, Status>> + Send>>;

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
            listeners: vec![format!("http://{}", info.listen)],
            started_unix_ms: info.started_unix_ms,
            apps: apps.len() as u32,
            releases: releases.len() as u32,
            edge_pid: info.edge_pid.load(std::sync::atomic::Ordering::Relaxed),
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
        let spec = resolve_spec(&self.handle, &request)?;
        let (created_tx, created_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Deploy {
                app: request.app.clone(),
                spec,
                id: (!request.release.is_empty()).then(|| request.release.clone()),
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
}

/// Resolve a deploy request into a release spec: the request's settings,
/// then the source's [tool.muru] table, then the application's previous
/// release, then the defaults of section 13.1.
fn resolve_spec(handle: &Handle, request: &pb::DeployRequest) -> Result<ReleaseSpec, Status> {
    let source = Path::new(&request.source);
    if !source.is_dir() {
        return Err(Status::invalid_argument(format!(
            "{} is not a directory",
            source.display()
        )));
    }
    let source = source
        .canonicalize()
        .map_err(|error| Status::invalid_argument(format!("{}: {error}", request.source)))?;
    let table = muru_config::load_project(&source)
        .map_err(|error| Status::invalid_argument(error.to_string()))?
        .map(|(_, table)| table)
        .unwrap_or_default();
    let previous = handle
        .store
        .app(&request.app)
        .ok()
        .and_then(|app| app.current.or(app.previous))
        .and_then(|id| handle.store.release(&request.app, &id).ok())
        .map(|release| release.spec);

    // The entrypoint from the request replaces the table's with its
    // factory settings, as muructl run does (section 13.1).
    let (entrypoint, from_table) = if !request.entrypoint.is_empty() {
        (request.entrypoint.clone(), false)
    } else if let Some(entrypoint) = table.entrypoint.clone() {
        (entrypoint, true)
    } else if let Some(previous) = &previous {
        (previous.entrypoint.clone(), false)
    } else {
        return Err(Status::invalid_argument(format!(
            "{} has no entrypoint in its [tool.muru] table; give one as module:callable",
            source.display()
        )));
    };
    let factory = match (request.factory, from_table) {
        (Some(true), _) => Some(parse_or(
            &request.factory_phase,
            muru_config::FactoryPhase::Worker,
        )?),
        (Some(false), _) => None,
        (None, true) if table.factory == Some(true) => Some(
            table
                .factory_phase
                .unwrap_or(muru_config::FactoryPhase::Worker),
        ),
        (None, _) => None,
    };

    let python = if !request.python.is_empty() {
        PathBuf::from(&request.python)
    } else {
        let venv = source.join(".venv").join("bin").join("python");
        if venv.is_file() {
            venv
        } else if let Some(previous) = &previous {
            previous.python.clone()
        } else {
            PathBuf::from("python3")
        }
    };

    Ok(ReleaseSpec {
        source,
        entrypoint,
        interface: match request.interface.as_str() {
            "" => table.interface.unwrap_or(muru_config::Interface::Auto),
            given => given.parse().map_err(|error: muru_config::ParseError| {
                Status::invalid_argument(error.to_string())
            })?,
        },
        python,
        processes: request
            .processes
            .or(table.processes.map(|value| value as u32))
            .or(previous.as_ref().map(|previous| previous.processes))
            .unwrap_or(1),
        threads: request
            .threads
            .or(table.threads.map(|value| value as u32))
            .or(previous.as_ref().map(|previous| previous.threads))
            .unwrap_or(4),
        sync_threads: request
            .sync_threads
            .or(table.sync_threads.map(|value| value as u32))
            .unwrap_or(8),
        concurrency: request
            .concurrency
            .or(table.concurrency.map(|value| value as u32))
            .unwrap_or(1000),
        preload: match request.preload.as_str() {
            "" => table.preload.unwrap_or(muru_config::Preload::Auto),
            given => given.parse().map_err(|error: muru_config::ParseError| {
                Status::invalid_argument(error.to_string())
            })?,
        },
        hooks: (!request.hooks.is_empty())
            .then(|| request.hooks.clone())
            .or(table.hooks),
        factory,
    })
}

fn parse_or<T: std::str::FromStr<Err = muru_config::ParseError>>(
    text: &str,
    default: T,
) -> Result<T, Status> {
    if text.is_empty() {
        return Ok(default);
    }
    text.parse()
        .map_err(|error: muru_config::ParseError| Status::invalid_argument(error.to_string()))
}

fn app_message(app: crate::model::App) -> pb::App {
    pb::App {
        name: app.name,
        current_release: app.current.unwrap_or_default(),
        previous_release: app.previous.unwrap_or_default(),
        resource_version: app.resource_version,
        created_unix_ms: app.created_unix_ms,
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
