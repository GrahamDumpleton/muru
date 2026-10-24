//! The minimal read-only dashboard of `muructl run` (design specification
//! section 10.7): workers, requests per second and latency over the last
//! minute, recent requests and logs, rendered on the server from askama
//! templates and refreshed by htmx polling. htmx is vendored and embedded,
//! so building needs only cargo.
//!
//! The caller provides a [`Source`] that produces a fresh [`Data`] for each
//! request; this crate knows nothing about how the figures are collected.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

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
#[derive(Debug, Clone, Default)]
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

/// Serve the dashboard on a bound listener until the task is dropped.
pub async fn serve(listener: TcpListener, source: Source) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let source = Arc::clone(&source);
                async move { Ok::<_, Infallible>(respond(&request, &source)) }
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

fn respond(request: &Request<Incoming>, source: &Source) -> Response<Full<Bytes>> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "only GET is served\n");
    }
    match request.uri().path() {
        "/static/htmx.min.js" => {
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
