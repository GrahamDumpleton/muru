//! The Muru edge: listeners, TLS, virtual hosts, routing, the request pipeline, upstreams, static files and uploads.
//!
//! Phase 1 scope so far: a listener speaking HTTP/1.1 and HTTP/2 (h2c by
//! prior knowledge; h2 over TLS arrives with TLS) that forwards every
//! request to one pool of workers over the worker link (design
//! specification section 5). The request head is sent as a typed frame, the
//! body streams behind it, and the response streams back; no HTTP is
//! re-serialised between the edge and the application. Routing, TLS and
//! everything else arrive later in phase 1 and beyond.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use http::header::{
    CONNECTION, CONTENT_LENGTH, EXPECT, HOST, HeaderMap, HeaderName, HeaderValue, SERVER,
};
use http::{Response, StatusCode, Version};
use http_body_util::combinators::UnsyncBoxBody;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use muru_proto::worker::v1 as pb;
use tokio::net::TcpListener;

pub mod access;
pub mod errors;
pub mod link;

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type ResponseBody = UnsyncBoxBody<Bytes, BoxError>;

/// What the edge listens on and which workers it forwards to.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address for the HTTP listener. Port 0 picks a free port.
    pub listen: SocketAddr,
    /// Unix socket paths of the workers.
    pub sockets: Vec<PathBuf>,
    /// Requests a worker handles at once, which bounds its connection pool.
    pub slots: usize,
    /// Development mode: error pages show tracebacks (section 4.12).
    pub development: bool,
    /// Where the access log goes and what it leaves out.
    pub access_log: access::Config,
    /// Timeouts and header limits.
    pub limits: Limits,
}

/// The limits and timeouts of sections 4.6 and 7.6, with their defaults.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Largest request head accepted, else 431.
    pub max_header_bytes: usize,
    /// Most headers accepted in a request, else 431.
    pub max_headers: usize,
    /// How long a client may take to send a complete request head.
    pub header_read_timeout: Duration,
    /// How long a request may wait for a worker, else 503 queue_full.
    pub queue_timeout: Duration,
    /// How long a request may take to produce its response head, else 504.
    pub request_deadline: Duration,
    /// How long a client may pause while sending a streamed body before the
    /// request is abandoned (408 if no response has started).
    pub body_idle_timeout: Duration,
    /// How long a worker may pause mid-response before it is cut off.
    pub response_idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_header_bytes: 64 * 1024,
            max_headers: 100,
            header_read_timeout: Duration::from_secs(30),
            queue_timeout: Duration::from_secs(5),
            request_deadline: Duration::from_secs(300),
            body_idle_timeout: Duration::from_secs(60),
            response_idle_timeout: Duration::from_secs(60),
        }
    }
}

/// Concurrent streams accepted per HTTP/2 connection (section 4.18).
const HTTP2_MAX_CONCURRENT_STREAMS: u32 = 128;

/// The worker pool requests go to, replaceable while the edge runs: a
/// reload switches to the new release's workers by swapping it (section
/// 9.8). Each request takes the pool current when it arrives.
pub struct Pools {
    current: RwLock<Arc<link::LinkPool>>,
}

impl Pools {
    /// The pool for a request arriving now.
    pub fn current(&self) -> Arc<link::LinkPool> {
        Arc::clone(&self.current.read().expect("pools poisoned"))
    }

    /// Send new requests to another set of workers. Requests in flight
    /// finish on the pool they started with.
    pub fn replace(&self, pool: link::LinkPool) {
        *self.current.write().expect("pools poisoned") = Arc::new(pool);
    }
}

/// A bound edge, ready to run.
pub struct Edge {
    listener: TcpListener,
    local: SocketAddr,
    pools: Arc<Pools>,
    request_ids: Arc<errors::RequestIds>,
    protocols: Arc<auto::Builder<TokioExecutor>>,
    development: bool,
    access_log: Arc<access::AccessLog>,
    limits: Arc<Limits>,
}

impl Edge {
    /// Bind the listener and prepare the worker pools.
    pub async fn bind(config: Config) -> anyhow::Result<Self> {
        let pools = Arc::new(Pools {
            current: RwLock::new(Arc::new(link::LinkPool::new(
                &config.sockets,
                config.slots,
            )?)),
        });
        let access_log = Arc::new(access::AccessLog::new(config.access_log)?);
        let listener = TcpListener::bind(config.listen).await?;
        let local = listener.local_addr()?;
        // Each connection is served as HTTP/1.1 or, when it opens with the
        // HTTP/2 preface, as HTTP/2.
        let limits = Arc::new(config.limits);
        let mut protocols = auto::Builder::new(TokioExecutor::new());
        protocols
            .http1()
            .timer(TokioTimer::new())
            .keep_alive(true)
            .max_headers(limits.max_headers)
            .max_buf_size(limits.max_header_bytes.max(8192))
            .header_read_timeout(limits.header_read_timeout);
        protocols
            .http2()
            .timer(TokioTimer::new())
            .max_concurrent_streams(HTTP2_MAX_CONCURRENT_STREAMS)
            .max_header_list_size(u32::try_from(limits.max_header_bytes).unwrap_or(u32::MAX));
        Ok(Self {
            listener,
            local,
            pools,
            request_ids: Arc::new(errors::RequestIds::new()),
            protocols: Arc::new(protocols),
            development: config.development,
            access_log,
            limits,
        })
    }

    /// The address the listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The access log, for the dashboard and the CLI.
    pub fn access_log(&self) -> Arc<access::AccessLog> {
        Arc::clone(&self.access_log)
    }

    /// The worker pools, for switching releases while running.
    pub fn pools(&self) -> Arc<Pools> {
        Arc::clone(&self.pools)
    }

    /// Accept connections until an accept error occurs.
    pub async fn run(self) -> anyhow::Result<()> {
        loop {
            let (stream, peer) = self.listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let pools = Arc::clone(&self.pools);
            let ids = Arc::clone(&self.request_ids);
            let protocols = Arc::clone(&self.protocols);
            let local = self.local;
            let development = self.development;
            let access_log = Arc::clone(&self.access_log);
            let limits = Arc::clone(&self.limits);
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let pool = pools.current();
                    let access_log = Arc::clone(&access_log);
                    let limits = Arc::clone(&limits);
                    let request_id = ids.next();
                    async move {
                        let context = link::Context {
                            request_id: &request_id,
                            peer,
                            local,
                            development,
                            access_log,
                            limits,
                        };
                        let mut response = link::handle(request, pool, &context).await;
                        // Section 4.19: every response names its request.
                        if let Ok(value) = HeaderValue::from_str(&request_id) {
                            response
                                .headers_mut()
                                .insert(errors::X_REQUEST_ID.clone(), value);
                        }
                        Ok::<_, Infallible>(response)
                    }
                });
                let connection = protocols.serve_connection(TokioIo::new(stream), service);
                if let Err(error) = connection.await {
                    tracing::debug!(%peer, %error, "connection ended with an error");
                }
            });
        }
    }
}

/// Headers that describe this connection rather than the request, which
/// never reach the application (section 4.7). Those named by a Connection
/// header are removed as well.
const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop(name: &str, named_in_connection: &[String]) -> bool {
    HOP_BY_HOP.contains(&name) || named_in_connection.iter().any(|named| named == name)
}

/// The headers a Connection header lists as hop-by-hop, lower-cased.
fn connection_options(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|option| option.trim().to_ascii_lowercase())
        .filter(|option| !option.is_empty())
        .collect()
}

/// Whether the client waits for 100 Continue before sending the body.
/// hyper ignores the expectation on HTTP/1.0, so the edge does too.
pub(crate) fn expects_continue(parts: &http::request::Parts) -> bool {
    parts.version > Version::HTTP_10
        && parts
            .headers
            .get(EXPECT)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"100-continue"))
}

pub(crate) fn request_head(
    parts: &http::request::Parts,
    request_id: &str,
    peer: SocketAddr,
    local: SocketAddr,
) -> pb::RequestHead {
    let content_length = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let named_in_connection = connection_options(&parts.headers);
    // HTTP/2 carries the host in :authority rather than a Host header, and
    // applications expect HTTP_HOST either way.
    let host = match (parts.headers.get(HOST), parts.uri.authority()) {
        (None, Some(authority)) => Some(pb::Header {
            name: Bytes::from_static(b"host"),
            value: Bytes::copy_from_slice(authority.as_str().as_bytes()),
        }),
        _ => None,
    };
    pb::RequestHead {
        protocol_version: pb::PROTOCOL_VERSION.to_owned(),
        request_id: request_id.to_owned(),
        method: parts.method.as_str().to_owned(),
        scheme: "http".to_owned(),
        raw_path: Bytes::copy_from_slice(parts.uri.path().as_bytes()),
        query: parts
            .uri
            .query()
            .map(|query| Bytes::copy_from_slice(query.as_bytes()))
            .unwrap_or_default(),
        headers: host
            .into_iter()
            .chain(parts.headers.iter().filter_map(|(name, value)| {
                // Section 4.7: a name with an underscore could masquerade as
                // another header's HTTP_ key, so it never reaches the
                // application, and neither does anything hop-by-hop.
                let name = name.as_str();
                if name.contains('_') || is_hop_by_hop(name, &named_in_connection) {
                    return None;
                }
                Some(pb::Header {
                    name: Bytes::copy_from_slice(name.as_bytes()),
                    value: Bytes::copy_from_slice(value.as_bytes()),
                })
            }))
            .collect(),
        client: Some(pb::Peer {
            address: peer.ip().to_string(),
            port: u32::from(peer.port()),
        }),
        server: Some(pb::Peer {
            address: local.ip().to_string(),
            port: u32::from(local.port()),
        }),
        content_length,
        http_version: format!("{:?}", parts.version),
        expect_continue: expects_continue(parts) && content_length != Some(0),
        ..Default::default()
    }
}

/// Map a worker status to an HTTP status code, with 500 for anything invalid.
pub(crate) fn status_code(status: u32) -> StatusCode {
    u16::try_from(status)
        .ok()
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// Copy the worker's headers into a response, dropping any that are invalid.
pub(crate) fn copy_headers(headers: &mut http::HeaderMap, from: Vec<pb::Header>) {
    for header in from {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(&header.name),
            HeaderValue::from_bytes(&header.value),
        ) {
            headers.append(name, value);
        }
    }
    headers.insert(SERVER, HeaderValue::from_static("muru"));
}

/// Mark a response as the last on its HTTP/1.1 connection, which hyper
/// closes once it has been written. An HTTP/2 stream needs nothing: the
/// connection carries on and an unread body resets only the stream.
pub(crate) fn close_after(
    mut response: Response<ResponseBody>,
    version: Version,
) -> Response<ResponseBody> {
    if version <= Version::HTTP_11 {
        response
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }
    response
}
