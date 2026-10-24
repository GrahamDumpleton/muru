//! The Muru edge: listeners, TLS, virtual hosts, routing, the request pipeline, upstreams, static files and uploads.
//!
//! Phase 0 scope: an HTTP/1.1 listener that forwards every request to one
//! pool of workers over the worker link (design specification section 5).
//! The request head is sent as a typed frame, the body streams behind it,
//! and the response streams back; no HTTP is re-serialised between the edge
//! and the application. Routing, TLS, HTTP/2 and everything else arrive in
//! phase 1 and later.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue, SERVER};
use http::{Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use muru_proto::worker::v1 as pb;
use tokio::net::TcpListener;

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
}

/// A bound edge, ready to run.
pub struct Edge {
    listener: TcpListener,
    local: SocketAddr,
    pool: Arc<link::LinkPool>,
    next_request_id: Arc<AtomicU64>,
}

impl Edge {
    /// Bind the listener and prepare the worker pools.
    pub async fn bind(config: Config) -> anyhow::Result<Self> {
        let pool = Arc::new(link::LinkPool::new(&config.sockets, config.slots)?);
        let listener = TcpListener::bind(config.listen).await?;
        let local = listener.local_addr()?;
        Ok(Self {
            listener,
            local,
            pool,
            next_request_id: Arc::new(AtomicU64::new(1)),
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
            let ids = Arc::clone(&self.next_request_id);
            let local = self.local;
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let pool = Arc::clone(&pool);
                    let request_id = ids.fetch_add(1, Ordering::Relaxed);
                    async move {
                        let response = link::handle(request, pool, request_id, peer, local).await;
                        Ok::<_, Infallible>(response)
                    }
                });
                let connection = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(TokioIo::new(stream), service);
                if let Err(error) = connection.await {
                    tracing::debug!(%peer, %error, "connection ended with an error");
                }
            });
        }
    }
}

pub(crate) fn request_head(
    parts: &http::request::Parts,
    request_id: u64,
    peer: SocketAddr,
    local: SocketAddr,
) -> pb::RequestHead {
    let content_length = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    pb::RequestHead {
        protocol_version: pb::PROTOCOL_VERSION.to_owned(),
        request_id: request_id.to_string(),
        method: parts.method.as_str().to_owned(),
        scheme: "http".to_owned(),
        raw_path: Bytes::copy_from_slice(parts.uri.path().as_bytes()),
        query: parts
            .uri
            .query()
            .map(|query| Bytes::copy_from_slice(query.as_bytes()))
            .unwrap_or_default(),
        headers: parts
            .headers
            .iter()
            // Section 4.7: a name with an underscore could masquerade as
            // another header's HTTP_ key, so it never reaches the application.
            .filter(|(name, _)| !name.as_str().contains('_'))
            .map(|(name, value)| pb::Header {
                name: Bytes::copy_from_slice(name.as_str().as_bytes()),
                value: Bytes::copy_from_slice(value.as_bytes()),
            })
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

pub(crate) fn error_response(status: StatusCode, message: &'static str) -> Response<ResponseBody> {
    let body = Full::new(Bytes::from(format!("{message}\n")))
        .map_err(|never: Infallible| match never {})
        .boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(SERVER, HeaderValue::from_static("muru"));
    response
}
