//! The read-only dashboard (design specification section 10.7): for a
//! `muructl run`, workers, requests per second and latency over the last
//! minute, recent requests and logs; for the daemon, the node, its
//! applications and releases, hosts and routes, requests, events and
//! logs, with the same figures as JSON on the gateway paths. Pages are
//! rendered on the server from askama templates and refreshed by htmx
//! polling. htmx is vendored and embedded, so building needs only cargo.
//!
//! The caller provides a [`Source`] or a [`NodeSource`] that produces
//! fresh data for each request; this crate knows nothing about how the
//! figures are collected.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use bytes::Bytes;
use http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

const HTMX: &[u8] = include_bytes!("../static/htmx.min.js");

/// Everything one rendering of the dashboard shows.
#[derive(Debug, Clone, Default)]
pub struct Data {
    /// The application name.
    pub app: String,
    /// The current release number.
    pub release: u32,
    /// How long the run has been up, already formatted.
    pub uptime: String,
    /// Whether this is a development run, which the header says.
    pub development: bool,
    /// The address the application is served on.
    pub listen: String,
    /// The HTTPS address, when TLS is on.
    pub https: Option<String>,
    /// Figures for the last minute.
    pub summary: Summary,
    /// One row per worker.
    pub workers: Vec<Worker>,
    /// Recent requests, newest first.
    pub requests: Vec<RequestRow>,
    /// Recent log lines, oldest first.
    pub logs: Vec<String>,
}

/// Figures for the last minute, already formatted.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Summary {
    /// Requests per second.
    pub rate: String,
    /// Median total time.
    pub p50: String,
    /// 95th percentile total time.
    pub p95: String,
    /// Responses with a 5xx status.
    pub errors: u64,
    /// Requests being handled right now.
    pub in_flight: usize,
}

/// One worker's row.
#[derive(Debug, Clone)]
pub struct Worker {
    /// The worker's index.
    pub index: usize,
    /// The link socket it listens on.
    pub link: String,
    /// Requests it can handle at once.
    pub slots: usize,
    /// Requests it is handling now.
    pub in_flight: usize,
}

/// One recent request, already formatted.
#[derive(Debug, Clone)]
pub struct RequestRow {
    /// Wall clock time, HH:MM:SS.
    pub time: String,
    /// The status code.
    pub status: u16,
    /// The first digit of the status, for colouring.
    pub status_class: u16,
    /// The edge's reason, when it generated the response.
    pub reason: String,
    /// The HTTP method.
    pub method: String,
    /// The path and query.
    pub target: String,
    /// Body bytes sent, formatted.
    pub bytes: String,
    /// Total time, formatted.
    pub total: String,
    /// Queue wait, formatted.
    pub queue: String,
    /// Application time, formatted.
    pub application: String,
    /// The worker index, or a dash.
    pub worker: String,
    /// The request identifier.
    pub request_id: String,
}

/// Produces the data for one rendering.
pub type Source = Arc<dyn Fn() -> Data + Send + Sync>;

/// A boxed future, as the daemon's sources return.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Produces the daemon dashboard's data for one rendering, or none when
/// the daemon is stopping.
pub type NodeSource = Arc<dyn Fn() -> BoxFuture<Option<NodeData>> + Send + Sync>;

/// Produces the Prometheus text exposition for the daemon's `/metrics`,
/// or none when the daemon is stopping.
pub type NodeMetricsSource = Arc<dyn Fn() -> BoxFuture<Option<String>> + Send + Sync>;

/// Everything the daemon's dashboard shows.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NodeData {
    /// The daemon's version.
    pub version: String,
    /// The daemon's pid.
    pub pid: u32,
    /// How long the daemon has been up, already formatted.
    pub uptime: String,
    /// The addresses the edge serves, as URLs.
    pub listeners: Vec<String>,
    /// The edge process's pid.
    pub edge_pid: u32,
    /// The routing snapshot in force.
    pub snapshot_version: u64,
    /// Where metrics are served, when they are.
    pub metrics: Option<String>,
    /// Figures for the last minute.
    pub summary: Summary,
    /// Applications with their releases.
    pub apps: Vec<AppRow>,
    /// Virtual hosts.
    pub hosts: Vec<HostRow>,
    /// Routes.
    pub routes: Vec<RouteRow>,
    /// Recent requests, newest first.
    pub requests: Vec<NodeRequestRow>,
    /// Recent events, newest first.
    pub events: Vec<EventRow>,
    /// The tail of each serving release's log.
    pub logs: Vec<LogPanel>,
}

/// An application and its releases.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AppRow {
    /// The name.
    pub name: String,
    /// The release traffic goes to, or empty.
    pub current: String,
    /// The release a rollback restores, or empty.
    pub previous: String,
    /// Its releases, newest first.
    pub releases: Vec<ReleaseRow>,
}

/// One release.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ReleaseRow {
    /// The release id.
    pub id: String,
    /// Its state, as section 9.1 names it.
    pub state: String,
    /// Whether traffic goes to it.
    pub serving: bool,
    /// Why it failed, or what it is doing.
    pub message: String,
    /// Workers listening.
    pub workers: usize,
    /// Slots handling a request.
    pub slots_busy: usize,
    /// Slots in all.
    pub slots_total: usize,
    /// Requests waiting for a slot.
    pub waiting: usize,
    /// When it was created, as a clock time.
    pub created: String,
}

/// A virtual host.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HostRow {
    /// The host's name.
    pub name: String,
    /// The names it answers to.
    pub names: Vec<String>,
    /// The listeners it is served on; every listener when empty.
    pub listeners: Vec<String>,
    /// Whether it has a certificate.
    pub tls: bool,
    /// Whether it is the default host.
    pub default: bool,
}

/// A route.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RouteRow {
    /// The host.
    pub host: String,
    /// The path prefix.
    pub path: String,
    /// The application.
    pub app: String,
}

/// One recent request under the daemon, already formatted.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NodeRequestRow {
    /// Wall clock time, HH:MM:SS.
    pub time: String,
    /// The status code.
    pub status: u16,
    /// The first digit of the status, for colouring.
    pub status_class: u16,
    /// The edge's reason, when it generated the response.
    pub reason: String,
    /// The HTTP method.
    pub method: String,
    /// The path and query.
    pub target: String,
    /// app@release, or a dash.
    pub release: String,
    /// The host.
    pub host: String,
    /// Body bytes sent, formatted.
    pub bytes: String,
    /// Total time, formatted.
    pub total: String,
    /// Queue wait, formatted.
    pub queue: String,
    /// Application time, formatted.
    pub application: String,
    /// The worker index, or a dash.
    pub worker: String,
    /// The request identifier.
    pub request_id: String,
}

/// One event.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EventRow {
    /// The sequence number.
    pub sequence: u64,
    /// Wall clock time, HH:MM:SS.
    pub time: String,
    /// info, notice, warning or error.
    pub level: String,
    /// The type, such as release.ready.
    pub kind: String,
    /// What it is about.
    pub subject: String,
    /// The sentence.
    pub message: String,
}

/// The tail of one release's log.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LogPanel {
    /// The application.
    pub app: String,
    /// The release.
    pub release: String,
    /// The last lines, oldest first.
    pub lines: Vec<String>,
}

/// An uptime as `1h 2m`, `3m 4s` or `5s`.
pub fn uptime(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();

    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// Produces the Prometheus text exposition for `/metrics` (section 11.1).
pub type MetricsSource = Arc<dyn Fn() -> String + Send + Sync>;

/// The content type of the Prometheus text exposition.
pub const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage<'a> {
    data: &'a Data,
}

#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewFragment<'a> {
    data: &'a Data,
}

#[derive(Template)]
#[template(path = "requests.html")]
struct RequestsFragment<'a> {
    data: &'a Data,
}

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsFragment<'a> {
    data: &'a Data,
}

#[derive(Template)]
#[template(path = "node.html")]
struct NodePage<'a> {
    data: &'a NodeData,
}

#[derive(Template)]
#[template(path = "node_overview.html")]
struct NodeOverviewFragment<'a> {
    data: &'a NodeData,
}

#[derive(Template)]
#[template(path = "node_requests.html")]
struct NodeRequestsFragment<'a> {
    data: &'a NodeData,
}

#[derive(Template)]
#[template(path = "node_events.html")]
struct NodeEventsFragment<'a> {
    data: &'a NodeData,
}

#[derive(Template)]
#[template(path = "node_logs.html")]
struct NodeLogsFragment<'a> {
    data: &'a NodeData,
}

/// Serve the dashboard on a bound listener until the task is dropped.
pub async fn serve(
    listener: TcpListener,
    source: Source,
    metrics: MetricsSource,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let source = Arc::clone(&source);
        let metrics = Arc::clone(&metrics);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let source = Arc::clone(&source);
                let metrics = Arc::clone(&metrics);
                async move { Ok::<_, Infallible>(respond(&request, &source, &metrics)) }
            });
            if let Err(error) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(%error, "dashboard connection ended with an error");
            }
        });
    }
}

/// Bind the dashboard on an address.
pub async fn bind(address: SocketAddr) -> anyhow::Result<TcpListener> {
    Ok(TcpListener::bind(address).await?)
}

fn respond(
    request: &Request<Incoming>,
    source: &Source,
    metrics: &MetricsSource,
) -> Response<Full<Bytes>> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "only GET is served\n");
    }

    match request.uri().path() {
        "/metrics" => metrics_response(metrics()),
        "/static/htmx.min.js" => htmx(),
        "/" => page(IndexPage { data: &source() }.render()),
        "/fragment/overview" => page(OverviewFragment { data: &source() }.render()),
        "/fragment/requests" => page(RequestsFragment { data: &source() }.render()),
        "/fragment/logs" => page(LogsFragment { data: &source() }.render()),
        _ => plain(StatusCode::NOT_FOUND, "not found\n"),
    }
}

fn page(rendered: Result<String, askama::Error>) -> Response<Full<Bytes>> {
    let body = match rendered {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(%error, "rendering the dashboard failed");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "rendering failed\n");
        }
    };

    let mut response = Response::new(Full::new(Bytes::from(body)));
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        "text/html; charset=utf-8".parse().expect("static"),
    );
    headers.insert(CACHE_CONTROL, "no-store".parse().expect("static"));

    // Section 10.7: a strict content security policy; the only script is
    // the embedded htmx, styles are inline.

    headers.insert(
        CONTENT_SECURITY_POLICY,
        "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self'"
            .parse()
            .expect("static"),
    );
    response
}

fn plain(status: StatusCode, text: &'static str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(text.as_bytes())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().expect("static"),
    );
    response
}

/// Serve the daemon's dashboard and gateway on a bound listener until
/// the task is dropped.
pub async fn serve_node(
    listener: TcpListener,
    source: NodeSource,
    metrics: NodeMetricsSource,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let source = Arc::clone(&source);
        let metrics = Arc::clone(&metrics);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let source = Arc::clone(&source);
                let metrics = Arc::clone(&metrics);
                async move { Ok::<_, Infallible>(respond_node(&request, &source, &metrics).await) }
            });

            if let Err(error) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(%error, "dashboard connection ended with an error");
            }
        });
    }
}

async fn respond_node(
    request: &Request<Incoming>,
    source: &NodeSource,
    metrics: &NodeMetricsSource,
) -> Response<Full<Bytes>> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "only GET is served\n");
    }

    let path = request.uri().path();

    if path == "/static/htmx.min.js" {
        return htmx();
    }

    if path == "/metrics" {
        return match metrics().await {
            Some(text) => metrics_response(text),
            None => plain(StatusCode::SERVICE_UNAVAILABLE, "the daemon is stopping\n"),
        };
    }

    let Some(data) = source().await else {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "the daemon is stopping\n");
    };

    match path {
        "/" => page(NodePage { data: &data }.render()),
        "/fragment/node/overview" => page(NodeOverviewFragment { data: &data }.render()),
        "/fragment/node/requests" => page(NodeRequestsFragment { data: &data }.render()),
        "/fragment/node/events" => page(NodeEventsFragment { data: &data }.render()),
        "/fragment/node/logs" => page(NodeLogsFragment { data: &data }.render()),
        "/v1/status" => json(&serde_json::json!({
            "version": data.version,
            "pid": data.pid,
            "uptime": data.uptime,
            "listeners": data.listeners,
            "edge_pid": data.edge_pid,
            "snapshot_version": data.snapshot_version,
            "metrics": data.metrics,
            "summary": data.summary,
        })),
        "/v1/apps" => json(&data.apps),
        "/v1/hosts" => json(&data.hosts),
        "/v1/routes" => json(&data.routes),
        "/v1/events" => json(&data.events),
        "/v1/requests" => json(&data.requests),
        _ => match path
            .strip_prefix("/v1/apps/")
            .and_then(|rest| rest.strip_suffix("/releases"))
        {
            Some(name) => match data.apps.iter().find(|app| app.name == name) {
                Some(app) => json(&app.releases),
                None => plain(StatusCode::NOT_FOUND, "no such application\n"),
            },
            None => plain(StatusCode::NOT_FOUND, "not found\n"),
        },
    }
}

fn json<T: serde::Serialize>(value: &T) -> Response<Full<Bytes>> {
    let body = match serde_json::to_vec(value) {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(%error, "encoding the gateway response failed");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed\n");
        }
    };

    let mut response = Response::new(Full::new(Bytes::from(body)));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, "application/json".parse().expect("static"));
    headers.insert(CACHE_CONTROL, "no-store".parse().expect("static"));
    response
}

fn htmx() -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(HTMX)));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, "text/javascript".parse().expect("static"));
    response.headers_mut().insert(
        CACHE_CONTROL,
        "public, max-age=86400".parse().expect("static"),
    );
    response
}

fn metrics_response(text: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(text)));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, METRICS_CONTENT_TYPE.parse().expect("static"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, "no-store".parse().expect("static"));
    response
}
