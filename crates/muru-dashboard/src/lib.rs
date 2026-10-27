//! The read-only dashboard (design specification section 10.7): for a
//! `muructl run`, workers, requests per second and latency over the last
//! minute, recent requests and logs; for the daemon, the node, its
//! applications and releases, hosts and routes, requests, events and
//! logs, with the same figures as JSON on the gateway paths. Pages are
//! rendered on the server from askama templates and refreshed by htmx
//! polling. htmx is vendored and embedded, so building needs only cargo.
//!
//! The caller provides a [`Source`] or a [`NodeSource`] that produces
//! fresh data for each request, and a [`History`] it records a point in
//! every second for the charts; this crate knows nothing about how the
//! figures are collected.

pub mod actions;
pub mod history;

pub use actions::{Action, ActionSink, Sessions};
pub use history::{History, Point, Reading, Sampler};

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
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

const HTMX: &[u8] = include_bytes!("../static/htmx.min.js");
const HTMX_SSE: &[u8] = include_bytes!("../static/sse.js");
const UPLOT_JS: &[u8] = include_bytes!("../static/uPlot.iife.min.js");
const UPLOT_CSS: &[u8] = include_bytes!("../static/uPlot.min.css");
const CHARTS_JS: &[u8] = include_bytes!("../static/charts.js");

/// A response body: whole, or a stream for Server-Sent Events.
type Body = UnsyncBoxBody<Bytes, Infallible>;

/// New events as they happen, for the events stream; the daemon gives
/// one, a development run has none.
pub type EventsSource = Arc<dyn Fn() -> tokio::sync::broadcast::Receiver<EventRow> + Send + Sync>;

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
    /// The WebSockets open now.
    pub websockets: WebSocketsView,
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
    /// Requests being handled right now, WebSockets not counted.
    pub in_flight: usize,
    /// WebSockets open right now.
    pub websockets: usize,
}

/// The WebSockets open now: counted by endpoint, and the longest open.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct WebSocketsView {
    /// How many are open.
    pub open: usize,
    /// Counted by endpoint, the most first.
    pub endpoints: Vec<EndpointRow>,
    /// The longest open, up to a limit.
    pub sockets: Vec<SocketRow>,
}

/// Open WebSockets of one endpoint: a path pattern where identifiers
/// are `{id}` and segments with many distinct values `*`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EndpointRow {
    /// The application.
    pub app: String,
    /// The path pattern.
    pub pattern: String,
    /// How many are open.
    pub open: usize,
}

/// One open WebSocket, already formatted.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SocketRow {
    /// app@release, or a dash.
    pub release: String,
    /// The path.
    pub path: String,
    /// The client address.
    pub client: String,
    /// When it opened, HH:MM:SS.
    pub opened: String,
    /// How long it has been open.
    pub age: String,
    /// Messages received from the client.
    pub received: u64,
    /// Messages sent to the client.
    pub sent: u64,
    /// The worker index, or a dash.
    pub worker: String,
    /// The request id of its handshake.
    pub request_id: String,
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

/// Signing in to the daemon's dashboard and acting from it (section
/// 10.7): the sessions signed in, and what carries an action out for one
/// that may act.
#[derive(Clone)]
pub struct Actions {
    /// Login codes and sessions, shared with the admin API.
    pub sessions: Arc<Sessions>,
    /// Carries an action out.
    pub sink: ActionSink,
}

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
    /// Each running release's worker groups as the autoscaler sees them
    /// (sections 7.3 and 7.4).
    pub scaling: Vec<ScalingRow>,
    /// Rollouts, those under way first, then the latest ended ones
    /// (section 9.3).
    pub rollouts: Vec<RolloutRow>,
    /// Recent requests, newest first.
    pub requests: Vec<NodeRequestRow>,
    /// The WebSockets open now.
    pub websockets: WebSocketsView,
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
    /// When maintenance mode began, as a clock time, or empty when it is
    /// off (section 4.12).
    pub maintenance_since: String,
    /// Its releases, newest first.
    pub releases: Vec<ReleaseRow>,
}

/// One worker group of a running release, as the autoscaler sees it.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScalingRow {
    /// app@release.
    pub release: String,
    /// The group.
    pub group: String,
    /// Workers ready, and the count being reached while they differ.
    pub workers: String,
    /// The policy as one phrase.
    pub policy: String,
    /// CPU per wall second as a share of what a worker can use,
    /// formatted, or a dash.
    pub cpu: String,
    /// The 95th percentile GIL wait, formatted, or a dash.
    pub gil_wait: String,
    /// The 95th percentile event loop lag, formatted, or a dash.
    pub loop_lag: String,
    /// The bottleneck in words.
    pub diagnosis: String,
    /// The tuning it suggests, or empty.
    pub advice: String,
    /// The last decision and how long ago, or empty.
    pub last_decision: String,
}

/// One rollout.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RolloutRow {
    /// The application.
    pub app: String,
    /// The rollout's id.
    pub id: String,
    /// The release gaining traffic.
    pub candidate: String,
    /// The release that served when it began.
    pub baseline: String,
    /// Its state: running, paused, awaiting_approval, promoted or
    /// rolled_back.
    pub state: String,
    /// The candidate's share in force, in percent.
    pub percent: u32,
    /// What it is doing, or why it ended.
    pub message: String,
    /// When it last changed, as a clock time.
    pub updated: String,
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
    /// Slots handling a request or holding a WebSocket.
    pub slots_busy: usize,
    /// WebSockets open, each holding a slot.
    pub websockets: usize,
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
    /// The signed-in session's form token, when actions are on.
    csrf: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "node_overview.html")]
struct NodeOverviewFragment<'a> {
    data: &'a NodeData,
    csrf: Option<&'a str>,
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

#[derive(Template)]
#[template(path = "node_event_row.html")]
struct NodeEventRow<'a> {
    event: &'a EventRow,
}

/// Serve the dashboard on a bound listener until the task is dropped.
/// `history` holds the points the charts draw.
pub async fn serve(
    listener: TcpListener,
    source: Source,
    metrics: MetricsSource,
    history: Arc<History>,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let source = Arc::clone(&source);
        let metrics = Arc::clone(&metrics);
        let history = Arc::clone(&history);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let source = Arc::clone(&source);
                let metrics = Arc::clone(&metrics);
                let history = Arc::clone(&history);
                async move { Ok::<_, Infallible>(respond(&request, &source, &metrics, &history)) }
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
    history: &Arc<History>,
) -> Response<Body> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "only GET is served\n");
    }

    if let Some(response) = asset_path(request.uri().path()) {
        return response;
    }

    if let Some(response) = history_paths(request, history) {
        return response;
    }

    match request.uri().path() {
        "/metrics" => metrics_response(metrics()),
        "/" => page(IndexPage { data: &source() }.render()),
        "/fragment/overview" => page(OverviewFragment { data: &source() }.render()),
        "/fragment/requests" => page(RequestsFragment { data: &source() }.render()),
        "/fragment/logs" => page(LogsFragment { data: &source() }.render()),
        "/v1/websockets" => json(&source().websockets),
        _ => plain(StatusCode::NOT_FOUND, "not found\n"),
    }
}

/// The embedded assets, which both dashboards serve to anyone: they are
/// the vendored libraries and the charts' script, nothing of the node's.
fn asset_path(path: &str) -> Option<Response<Body>> {
    Some(match path {
        "/static/htmx.min.js" => asset(HTMX, "text/javascript"),
        "/static/sse.js" => asset(HTMX_SSE, "text/javascript"),
        "/static/uplot.min.js" => asset(UPLOT_JS, "text/javascript"),
        "/static/uplot.min.css" => asset(UPLOT_CSS, "text/css"),
        "/static/charts.js" => asset(CHARTS_JS, "text/javascript"),
        _ => return None,
    })
}

/// The history and its stream, which both dashboards serve the same way.
fn history_paths(request: &Request<Incoming>, history: &Arc<History>) -> Option<Response<Body>> {
    Some(match request.uri().path() {
        "/v1/history" => {
            let daily = request
                .uri()
                .query()
                .is_some_and(|query| query.split('&').any(|pair| pair == "range=24h"));
            let points: Vec<serde_json::Value> =
                history.points(daily).iter().map(Point::to_json).collect();
            json(&serde_json::json!({
                "range": if daily { "24h" } else { "1h" },
                "step_s": if daily { 60 } else { 1 },
                "points": points,
            }))
        }
        "/v1/history/stream" => {
            // The receiver is the stream's state, carried from one message
            // to the next.

            sse(futures::stream::unfold(
                history.subscribe(),
                |mut live| async move {
                    let chunk = loop {
                        match tokio::time::timeout(Duration::from_secs(15), live.recv()).await {
                            Ok(Ok(point)) => {
                                break format!("event: point\ndata: {}\n\n", point.to_json());
                            }
                            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                                continue;
                            }
                            Ok(Err(_)) => return None,
                            Err(_) => break ": keep-alive\n\n".to_owned(),
                        }
                    };

                    Some((chunk, live))
                },
            ))
        }
        _ => return None,
    })
}

fn page(rendered: Result<String, askama::Error>) -> Response<Body> {
    let body = match rendered {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(%error, "rendering the dashboard failed");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "rendering failed\n");
        }
    };

    let mut response = Response::new(whole(Bytes::from(body)));
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        "text/html; charset=utf-8".parse().expect("static"),
    );
    headers.insert(CACHE_CONTROL, "no-store".parse().expect("static"));

    // Section 10.7: a strict content security policy; the only scripts
    // and stylesheets are the embedded ones, the page's own styles are
    // inline, and the streams connect back to the same origin. A page of
    // the same origin may show the dashboard in a frame, as JupyterLab
    // does through jupyter-server-proxy; no other site may.

    headers.insert(
        CONTENT_SECURITY_POLICY,
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'self'"
            .parse()
            .expect("static"),
    );
    response
}

fn whole(bytes: Bytes) -> Body {
    Full::new(bytes).boxed_unsync()
}

fn plain(status: StatusCode, text: &'static str) -> Response<Body> {
    plain_text(status, text.to_owned())
}

fn plain_text(status: StatusCode, text: String) -> Response<Body> {
    let mut response = Response::new(whole(Bytes::from(text)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().expect("static"),
    );
    response
}

/// Serve the daemon's dashboard and gateway on a bound listener until
/// the task is dropped. `history` holds the points the charts draw and
/// `events` gives the events as they happen, for the events stream.
pub async fn serve_node(
    listener: TcpListener,
    source: NodeSource,
    metrics: NodeMetricsSource,
    history: Arc<History>,
    events: EventsSource,
    actions: Actions,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let source = Arc::clone(&source);
        let metrics = Arc::clone(&metrics);
        let history = Arc::clone(&history);
        let events = Arc::clone(&events);
        let actions = actions.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let source = Arc::clone(&source);
                let metrics = Arc::clone(&metrics);
                let history = Arc::clone(&history);
                let events = Arc::clone(&events);
                let actions = actions.clone();
                async move {
                    Ok::<_, Infallible>(
                        respond_node(request, &source, &metrics, &history, &events, actions).await,
                    )
                }
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
    request: Request<Incoming>,
    source: &NodeSource,
    metrics: &NodeMetricsSource,
    history: &Arc<History>,
    events: &EventsSource,
    actions: Actions,
) -> Response<Body> {
    // Section 10.7: signing in, and the actions for a session that may
    // act.

    if request.method() == Method::POST && request.uri().path() == "/actions/maintenance" {
        return act(request, &actions).await;
    }

    if request.method() != Method::GET && request.method() != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "only GET is served\n");
    }

    let request = &request;
    let path = request.uri().path();

    if path == "/login" {
        return login(request, &actions);
    }

    // The metrics are for Prometheus, which cannot sign in, and the
    // assets hold nothing of the node's; everything else needs a session.

    if path == "/metrics" {
        return match metrics().await {
            Some(text) => metrics_response(text),
            None => plain(StatusCode::SERVICE_UNAVAILABLE, "the daemon is stopping\n"),
        };
    }

    if let Some(response) = asset_path(path) {
        return response;
    }

    let Some((session, _)) = signed_in(request, &actions) else {
        return sign_in_first(path);
    };
    let csrf = session.actions.then_some(session.csrf);

    if let Some(response) = history_paths(request, history) {
        return response;
    }

    if path == "/v1/events/stream" {
        // Each event as a rendered row for the page's table, and as JSON
        // in the same message's `json` field for scripts.

        return sse(futures::stream::unfold(events(), |mut live| async move {
            let chunk = loop {
                match tokio::time::timeout(Duration::from_secs(15), live.recv()).await {
                    Ok(Ok(event)) => {
                        let row = NodeEventRow { event: &event }
                            .render()
                            .unwrap_or_default()
                            .replace('\n', "");
                        break format!(
                            "event: event\ndata: {row}\n\nevent: json\ndata: {}\n\n",
                            serde_json::to_string(&event).unwrap_or_default()
                        );
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(Err(_)) => return None,
                    Err(_) => break ": keep-alive\n\n".to_owned(),
                }
            };

            Some((chunk, live))
        }));
    }

    let Some(data) = source().await else {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "the daemon is stopping\n");
    };

    match path {
        "/" => page(
            NodePage {
                data: &data,
                csrf: csrf.as_deref(),
            }
            .render(),
        ),
        "/fragment/node/overview" => page(
            NodeOverviewFragment {
                data: &data,
                csrf: csrf.as_deref(),
            }
            .render(),
        ),
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
        "/v1/scaling" => json(&data.scaling),
        "/v1/rollouts" => json(&data.rollouts),
        "/v1/events" => json(&data.events),
        "/v1/requests" => json(&data.requests),
        "/v1/websockets" => json(&data.websockets),
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

/// The live session a request names, by a bearer token or its cookie,
/// and whether it came as a bearer token.
fn signed_in(request: &Request<Incoming>, actions: &Actions) -> Option<(actions::Session, bool)> {
    let bearer = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    if let Some(token) = bearer {
        return actions
            .sessions
            .session(token.trim())
            .map(|session| (session, true));
    }

    request
        .headers()
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == actions::SESSION_COOKIE)
        .and_then(|(_, id)| actions.sessions.session(id.trim()))
        .map(|session| (session, false))
}

/// What a request without a session gets: a page saying how to sign in
/// for the dashboard itself, and a plain refusal for the rest.
fn sign_in_first(path: &str) -> Response<Body> {
    if path != "/" {
        return plain(
            StatusCode::UNAUTHORIZED,
            "not signed in; muructl dashboard login gives a link, or with --token a token for scripts\n",
        );
    }

    let mut response = page(Ok(SIGN_IN_PAGE.to_owned()));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
}

/// The page a browser without a session is shown.
const SIGN_IN_PAGE: &str = "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<title>Muru: sign in</title>
<style>
:root { color-scheme: light dark; }
body { margin: 0; font: 15px/1.45 system-ui, sans-serif; }
main { max-width: 36rem; margin: 4rem auto; padding: 0 1.5rem; }
code { font-size: 0.95em; }
</style>
</head>
<body>
<main>
<h1>Sign in to the Muru dashboard</h1>
<p>Run <code>muructl dashboard login</code> where the daemon runs, and open the link it prints within five minutes.
Add <code>--actions</code> for a session that may act, such as turning maintenance mode on and off.</p>
</main>
</body>
</html>
";

/// Redeem a login link: a session cookie and back to the page.
fn login(request: &Request<Incoming>, actions: &Actions) -> Response<Body> {
    let code = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| pair.strip_prefix("code="))
        .unwrap_or_default();

    let Some((id, _)) = actions.sessions.redeem(code) else {
        return plain(
            StatusCode::FORBIDDEN,
            "this login link is unknown, used or expired; muructl dashboard login gives another\n",
        );
    };

    // The page is wherever the link was opened from, so the dashboard
    // works under a proxy's path prefix, and the cookie, with no Path,
    // is kept for that prefix alone.

    let mut response = plain(StatusCode::SEE_OTHER, "signed in\n");
    let headers = response.headers_mut();
    headers.insert(http::header::LOCATION, "./".parse().expect("static"));

    if let Ok(cookie) = format!(
        "{}={id}; Max-Age=43200; HttpOnly; SameSite=Strict",
        actions::SESSION_COOKIE
    )
    .parse()
    {
        headers.insert(http::header::SET_COOKIE, cookie);
    }

    response
}

/// Carry out an action for a live session that may act: a form posted by
/// a page of the dashboard's own origin with the session's token, or a
/// script's request with its bearer token.
async fn act(request: Request<Incoming>, actions: &Actions) -> Response<Body> {
    let Some((session, bearer)) = signed_in(&request, actions) else {
        return plain(
            StatusCode::FORBIDDEN,
            "not signed in; muructl dashboard login --actions gives a link\n",
        );
    };

    if !session.actions {
        return plain(
            StatusCode::FORBIDDEN,
            "this session may only look; muructl dashboard login --actions gives one that may act\n",
        );
    }

    // A browser sends the cookie whoever's page posts, so a form must come
    // from the dashboard's own origin and carry the session's token; a
    // bearer token is never sent by a browser on its own.

    if !bearer && !same_origin(&request) {
        return plain(
            StatusCode::FORBIDDEN,
            "an action must come from the dashboard's own page\n",
        );
    }

    let body = match http_body_util::Limited::new(request.into_body(), 4096)
        .collect()
        .await
    {
        Ok(body) => body.to_bytes(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "the form is too large\n"),
    };
    let fields = actions::form_fields(&body);

    if !bearer && !actions::same_token(fields.get("csrf").map_or("", String::as_str), &session.csrf)
    {
        return plain(
            StatusCode::FORBIDDEN,
            "the form's token is not this session's\n",
        );
    }

    let action = Action::Maintenance {
        app: fields.get("app").cloned().unwrap_or_default(),
        on: fields.get("state").map(String::as_str) == Some("on"),
    };

    match (actions.sink)(action, session.label).await {
        Ok(_) => {
            // Back to the page the form was on, which is one level up from
            // the action's own path, under whatever prefix it is served.

            let mut response = plain(StatusCode::SEE_OTHER, "done\n");
            response
                .headers_mut()
                .insert(http::header::LOCATION, "../".parse().expect("static"));
            response
        }
        Err(error) => plain_text(StatusCode::CONFLICT, format!("{error}\n")),
    }
}

/// Whether a form was posted by a page of the dashboard's own origin. The
/// browser's Sec-Fetch-Site says so whatever proxy is in front, since it
/// compares the page with the URL the browser asked for; without it, the
/// Origin must be the Host the request names.
fn same_origin(request: &Request<Incoming>) -> bool {
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    };

    if let Some(site) = header("sec-fetch-site") {
        return site == "same-origin";
    }

    match (header("origin"), header("host")) {
        (Some(origin), Some(host)) => {
            origin == format!("http://{host}") || origin == format!("https://{host}")
        }
        _ => false,
    }
}

/// A Server-Sent Events response from a stream of messages.
fn sse<S>(messages: S) -> Response<Body>
where
    S: futures::Stream<Item = String> + Send + 'static,
{
    use futures::StreamExt as _;

    let frames = messages.map(|message| Ok::<_, Infallible>(Frame::data(Bytes::from(message))));
    let mut response = Response::new(BodyExt::boxed_unsync(StreamBody::new(frames)));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, "text/event-stream".parse().expect("static"));
    headers.insert(CACHE_CONTROL, "no-store".parse().expect("static"));
    headers.insert(
        http::header::HeaderName::from_static("x-accel-buffering"),
        "no".parse().expect("static"),
    );
    response
}

fn json<T: serde::Serialize>(value: &T) -> Response<Body> {
    let body = match serde_json::to_vec(value) {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(%error, "encoding the gateway response failed");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed\n");
        }
    };

    let mut response = Response::new(whole(Bytes::from(body)));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, "application/json".parse().expect("static"));
    headers.insert(CACHE_CONTROL, "no-store".parse().expect("static"));
    response
}

fn asset(bytes: &'static [u8], content_type: &'static str) -> Response<Body> {
    let mut response = Response::new(whole(Bytes::from_static(bytes)));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, content_type.parse().expect("static"));
    response.headers_mut().insert(
        CACHE_CONTROL,
        "public, max-age=86400".parse().expect("static"),
    );
    response
}

fn metrics_response(text: String) -> Response<Body> {
    let mut response = Response::new(whole(Bytes::from(text)));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, METRICS_CONTENT_TYPE.parse().expect("static"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, "no-store".parse().expect("static"));
    response
}
