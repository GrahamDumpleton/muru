//! `muructl serve`'s admin port and probe socket (section 13.9): the
//! probes a container platform asks, the Prometheus metrics and, when
//! asked for, a read-only status. The same answers are served on a TCP
//! listener for the platform and on a Unix socket in the run directory
//! for `muructl probe`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use muru_edge::Pools;
use muru_edge::access::AccessLog;

/// How long the host may go without a word before `/livez` fails: the
/// host reports about once a second while its workers report to it, and
/// replaces a worker that stalls well within this.
const HOST_SILENCE: Duration = Duration::from_secs(30);

/// What the probes look at, shared with the serving code that fills it.
pub struct Health {
    /// The workers ready in every required group for `/readyz`.
    min_ready: usize,
    /// Whether `/status` is served.
    status: bool,
    /// Set once preload has finished and the first workers are ready.
    started: AtomicBool,
    /// Set when SIGTERM or SIGINT arrives.
    stopping: AtomicBool,
    /// When the host was last heard from, Unix milliseconds.
    host_heard: AtomicU64,
    /// What is served, once it is.
    serving: OnceLock<Serving>,
}

/// The running application, as the probes and metrics see it.
pub struct Serving {
    /// The worker pools.
    pub pools: Arc<Pools>,
    /// The access log, which holds the request metrics.
    pub access_log: Arc<AccessLog>,
    /// The listeners' connection counts.
    pub connections: muru_edge::ConnectionCounts,
    /// The groups that must have workers ready; an optional group, which
    /// may have none, is not among them.
    pub required: Vec<String>,
    /// The settings worked out, for `/status`.
    pub settings: serde_json::Value,
}

impl Health {
    /// Probes that require `min_ready` workers, with `/status` when
    /// `status` is set.
    pub fn new(min_ready: usize, status: bool) -> Arc<Self> {
        Arc::new(Self {
            min_ready,
            status,
            started: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            host_heard: AtomicU64::new(now_ms()),
            serving: OnceLock::new(),
        })
    }

    /// The application is served: the probes may pass from now on.
    pub fn started(&self, serving: Serving) {
        let _ = self.serving.set(serving);
        self.started.store(true, Ordering::Relaxed);
    }

    /// The host has reported: it is alive.
    pub fn host_heard(&self) {
        self.host_heard.store(now_ms(), Ordering::Relaxed);
    }

    /// Whether the host has been heard from recently. The edge answering
    /// this at all is the other half of liveness.
    fn live(&self) -> Result<(), String> {
        let silent = now_ms().saturating_sub(self.host_heard.load(Ordering::Relaxed));

        if self.started.load(Ordering::Relaxed) && silent > HOST_SILENCE.as_millis() as u64 {
            return Err(format!("the host has not reported for {} s", silent / 1000));
        }

        Ok(())
    }

    /// The container is stopping: readiness fails from now on.
    pub fn stopping(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    /// Whether every required group has `min_ready` workers, and if not,
    /// which does not.
    fn ready(&self) -> Result<(), String> {
        if self.stopping.load(Ordering::Relaxed) {
            return Err("stopping".to_owned());
        }

        let Some(serving) = self.serving.get() else {
            return Err("starting".to_owned());
        };

        let routing = serving.pools.current();

        for (group, pool) in routing.default_pools() {
            if !serving.required.contains(&group) {
                continue;
            }

            let ready = pool.workers().len();

            if ready < self.min_ready {
                return Err(format!(
                    "group {group} has {ready} of {} workers ready",
                    self.min_ready
                ));
            }
        }

        Ok(())
    }

    fn answer(&self, path: &str) -> (StatusCode, &'static str, String) {
        let text = "text/plain; charset=utf-8";

        match path {
            "/livez" => match self.live() {
                Ok(()) => (StatusCode::OK, text, "ok\n".to_owned()),
                Err(reason) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    text,
                    format!("not live: {reason}\n"),
                ),
            },
            "/startupz" if self.started.load(Ordering::Relaxed) => {
                (StatusCode::OK, text, "ok\n".to_owned())
            }
            "/startupz" => (
                StatusCode::SERVICE_UNAVAILABLE,
                text,
                "starting\n".to_owned(),
            ),
            "/readyz" => match self.ready() {
                Ok(()) => (StatusCode::OK, text, "ok\n".to_owned()),
                Err(reason) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    text,
                    format!("not ready: {reason}\n"),
                ),
            },
            "/metrics" => match self.serving.get() {
                Some(serving) => (
                    StatusCode::OK,
                    muru_controller::metrics::TEXT_FORMAT,
                    crate::run_admin::metrics_text(
                        &serving.pools,
                        &serving.access_log,
                        &serving.connections,
                    ),
                ),
                None => (
                    StatusCode::OK,
                    muru_controller::metrics::TEXT_FORMAT,
                    String::new(),
                ),
            },
            "/status" if self.status => (
                StatusCode::OK,
                "application/json",
                self.status_json().to_string() + "\n",
            ),
            _ => (
                StatusCode::NOT_FOUND,
                text,
                "not found; this port serves /livez, /readyz, /startupz and /metrics\n".to_owned(),
            ),
        }
    }

    fn status_json(&self) -> serde_json::Value {
        let Some(serving) = self.serving.get() else {
            return serde_json::json!({"state": "starting"});
        };

        let routing = serving.pools.current();
        let groups: serde_json::Map<String, serde_json::Value> = routing
            .default_pools()
            .into_iter()
            .map(|(group, pool)| {
                let workers: Vec<serde_json::Value> = pool
                    .workers()
                    .iter()
                    .map(|worker| {
                        serde_json::json!({
                            "in_flight": worker.in_flight,
                        })
                    })
                    .collect();
                (
                    group,
                    serde_json::json!({ "ready": workers.len(), "workers": workers }),
                )
            })
            .collect();

        serde_json::json!({
            "state": if self.stopping.load(Ordering::Relaxed) { "stopping" } else { "serving" },
            "groups": groups,
            "settings": serving.settings,
        })
    }
}

/// Serve the probes on a TCP address, returning the address bound.
pub async fn listen(address: SocketAddr, health: Arc<Health>) -> anyhow::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| anyhow::anyhow!("the admin port {address} cannot be bound: {error}"))?;
    let bound = listener.local_addr()?;

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(connection(TokioIo::new(stream), Arc::clone(&health)));
        }
    });

    Ok(bound)
}

/// Serve the probes on a Unix socket, for `muructl probe`.
pub fn listen_unix(path: &Path, health: Arc<Health>) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)
        .map_err(|error| anyhow::anyhow!("{} cannot be bound: {error}", path.display()))?;

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(connection(TokioIo::new(stream), Arc::clone(&health)));
        }
    });

    Ok(())
}

async fn connection<I>(io: I, health: Arc<Health>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let service = service_fn(move |request: Request<Incoming>| {
        let health = Arc::clone(&health);
        async move { Ok::<_, std::convert::Infallible>(respond(&request, &health)) }
    });

    let _ = http1::Builder::new().serve_connection(io, service).await;
}

fn respond(request: &Request<Incoming>, health: &Health) -> Response<Full<Bytes>> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Full::new(Bytes::from_static(b"only GET is served\n")))
            .expect("a valid response");
    }

    let (status, content_type, body) = health.answer(request.uri().path());

    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(body)))
        .expect("a valid response")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}
