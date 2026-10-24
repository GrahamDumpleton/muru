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
use std::sync::Arc;

use bytes::Bytes;
use http::header::{
    CONNECTION, CONTENT_LENGTH, EXPECT, HOST, HeaderMap, HeaderName, HeaderValue, SERVER,
};
use http::{Response, StatusCode, Version};
use http_body_util::combinators::UnsyncBoxBody;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use muru_proto::worker::v1 as pb;
use tokio::net::TcpListener;

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
}

/// Concurrent streams accepted per HTTP/2 connection (section 4.18).
const HTTP2_MAX_CONCURRENT_STREAMS: u32 = 128;

/// A bound edge, ready to run.
pub struct Edge {
    listener: TcpListener,
    local: SocketAddr,
    pool: Arc<link::LinkPool>,
    request_ids: Arc<errors::RequestIds>,
    protocols: Arc<auto::Builder<TokioExecutor>>,
    development: bool,
}

impl Edge {
    /// Bind the listener and prepare the worker pools.
    pub async fn bind(config: Config) -> anyhow::Result<Self> {
        let pool = Arc::new(link::LinkPool::new(&config.sockets, config.slots)?);
        let listener = TcpListener::bind(config.listen).await?;
        let local = listener.local_addr()?;
        // Each connection is served as HTTP/1.1 or, when it opens with the
        // HTTP/2 preface, as HTTP/2.
        let mut protocols = auto::Builder::new(TokioExecutor::new());
        protocols.http1().keep_alive(true);
        protocols
            .http2()
            .max_concurrent_streams(HTTP2_MAX_CONCURRENT_STREAMS);
        Ok(Self {
            listener,
            local,
            pool,
            request_ids: Arc::new(errors::RequestIds::new()),
            protocols: Arc::new(protocols),
            development: config.development,
        })
    }

    /// The address the listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Accept connections until an accept error occurs.
    pub async fn run(self) -> anyhow::Result<()> {
        loop {
            let (stream, peer) = self.listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let pool = Arc::clone(&self.pool);
            let ids = Arc::clone(&self.request_ids);
            let protocols = Arc::clone(&self.protocols);
            let local = self.local;
            let development = self.development;
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let pool = Arc::clone(&pool);
                    let request_id = ids.next();
                    async move {
                        let mut response =
                            link::handle(request, pool, &request_id, peer, local, development)
                                .await;
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
