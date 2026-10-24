//! The Muru edge: listeners, TLS, virtual hosts, routing, the request pipeline, upstreams, static files and uploads.
//!
//! Phase 1 scope so far: an HTTP listener and an optional HTTPS listener,
//! each speaking HTTP/1.1 and HTTP/2 (h2c by prior knowledge, h2 by ALPN
//! over TLS), forwarding every request to one pool of workers over the
//! worker link (design specification section 5). The request head is sent
//! as a typed frame, the body streams behind it, and the response streams
//! back; no HTTP is re-serialised between the edge and the application.
//! Routing, virtual hosts and everything else arrive in phase 2 and beyond.

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
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use muru_proto::worker::v1 as pb;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

pub mod access;
pub mod compression;
pub mod errors;
pub mod link;
pub mod metrics;
pub mod proxy;
pub mod routing;
pub mod static_files;
pub mod tls;
pub mod websocket;

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type ResponseBody = UnsyncBoxBody<Bytes, BoxError>;

/// What the edge listens on and which workers it forwards to.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address for the HTTP listener. Port 0 picks a free port.
    pub listen: SocketAddr,
    /// The application's name, as the access log labels its requests;
    /// empty when a snapshot will name it.
    pub app: String,
    /// The release the workers serve, for the access log; empty when a
    /// snapshot will name it.
    pub release: String,
    /// Unix socket paths of the default group's workers. Each worker's
    /// HELLO says how many requests it takes at once.
    pub sockets: Vec<PathBuf>,
    /// The named worker groups, with the paths that select them.
    pub groups: Vec<routing::GroupSockets>,
    /// The named mounts, with the paths that select them.
    pub mounts: Vec<routing::MountPath>,
    /// Development mode: error pages show tracebacks (section 4.12).
    pub development: bool,
    /// Where the access log goes and what it leaves out.
    pub access_log: access::Config,
    /// Timeouts and header limits.
    pub limits: Limits,
    /// How responses are compressed (section 4.17).
    pub compression: compression::Compression,
    /// The HTTPS listener, when TLS is on.
    pub https: Option<Https>,
}

/// An HTTPS listener next to the HTTP one (section 13.1 in development).
#[derive(Debug, Clone)]
pub struct Https {
    /// Address for the HTTPS listener. Port 0 picks a free port.
    pub listen: SocketAddr,
    /// The TLS configuration, from [`tls::server_config`].
    pub config: Arc<rustls::ServerConfig>,
    /// Redirect plain HTTP requests here instead of serving them.
    pub only: bool,
}

/// The limits and timeouts of sections 4.6 and 7.6, with their defaults.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Largest request body accepted, else 413 (max_body_size in section
    /// 4.6; 10 MB by ADR 0008, 100 MB in development).
    pub max_body_size: u64,
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
    /// Largest WebSocket message accepted from a client, else close 1009
    /// (websocket.max_message_size in section 4.18).
    pub websocket_max_message_size: usize,
    /// How often an idle WebSocket client is pinged.
    pub websocket_ping_interval: Duration,
    /// How long a WebSocket may stay silent before it is closed with 1001.
    pub websocket_idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_size: 10 * 1024 * 1024,
            max_header_bytes: 64 * 1024,
            max_headers: 100,
            header_read_timeout: Duration::from_secs(30),
            queue_timeout: Duration::from_secs(5),
            request_deadline: Duration::from_secs(300),
            body_idle_timeout: Duration::from_secs(60),
            response_idle_timeout: Duration::from_secs(60),
            websocket_max_message_size: 1024 * 1024,
            websocket_ping_interval: Duration::from_secs(30),
            websocket_idle_timeout: Duration::from_secs(300),
        }
    }
}

/// Concurrent streams accepted per HTTP/2 connection (section 4.18).
const HTTP2_MAX_CONCURRENT_STREAMS: u32 = 128;

/// The routing tables requests go through, replaceable while the edge
/// runs: a reload or a snapshot swaps them (sections 4.4 and 9.8). Each
/// request takes the tables current when it arrives.
pub struct Pools {
    current: RwLock<Arc<routing::Routing>>,
    /// The compression policy a development run keeps across replacements.
    compression: compression::Compression,
}

impl Pools {
    /// The tables for a request arriving now.
    pub fn current(&self) -> Arc<routing::Routing> {
        Arc::clone(&self.current.read().expect("pools poisoned"))
    }

    /// Send new requests to another application's workers, with no
    /// hosts. Requests in flight finish on the pool they started with.
    pub fn replace(&self, pools: routing::AppPools) {
        self.apply(routing::Routing::single(pools).with_compression(self.compression.clone()));
    }

    /// Switch to the tables of a new snapshot.
    pub fn apply(&self, routing: routing::Routing) {
        *self.current.write().expect("pools poisoned") = Arc::new(routing);
    }
}

/// A listener handed to the edge by the daemon, with whether it carries TLS.
pub struct Inherited {
    /// The bound, listening socket.
    pub listener: std::net::TcpListener,
    /// Terminate TLS with the certificates of the snapshot's hosts.
    pub tls: bool,
}

/// A listener could not be bound, most often because the port is in use.
#[derive(Debug, thiserror::Error)]
#[error("cannot listen on {address}: {source}")]
pub struct BindError {
    /// The address that could not be bound.
    pub address: SocketAddr,
    /// What the operating system said.
    #[source]
    pub source: std::io::Error,
}

/// What every connection shares.
struct Shared {
    pools: Arc<Pools>,
    request_ids: errors::RequestIds,
    protocols: auto::Builder<TokioExecutor>,
    development: bool,
    access_log: Arc<access::AccessLog>,
    limits: Arc<Limits>,
    /// Where plain HTTP requests are redirected instead of being served.
    redirect_to: Option<SocketAddr>,
    /// The port of the first TLS listener, for hosts that insist on HTTPS.
    https_port: Option<u16>,
    /// Set once the edge is draining, which stops the accept loops and
    /// tells every connection to finish.
    draining: tokio::sync::watch::Sender<bool>,
    /// Cloned for each connection accepted and dropped when it ends, so
    /// a drain can wait for the count to come back to one.
    connections: Arc<()>,
}

/// Stops an edge accepting and lets its connections finish (section
/// 13.4): what the daemon asks of an edge it replaces or shuts down.
#[derive(Clone)]
pub struct Drainer {
    shared: Arc<Shared>,
}

impl Drainer {
    /// Stop accepting, let the requests in flight finish and close idle
    /// keep-alive connections (with GOAWAY on HTTP/2), waiting at most
    /// `grace` for them. Returns whether every connection finished in
    /// time; connections handed over to WebSockets are not waited for.
    pub async fn drain(&self, grace: Duration) -> bool {
        let _ = self.shared.draining.send(true);
        let deadline = tokio::time::Instant::now() + grace;

        while Arc::strong_count(&self.shared.connections) > 1 {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }

            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        true
    }
}

/// A bound edge, ready to run.
pub struct Edge {
    listeners: Vec<(TcpListener, SocketAddr, Option<TlsAcceptor>)>,
    local: SocketAddr,
    shared: Arc<Shared>,
}

impl Edge {
    /// Bind the listeners and prepare the worker pools.
    pub async fn bind(config: Config) -> anyhow::Result<Self> {
        let http = TcpListener::bind(config.listen)
            .await
            .map_err(|source| BindError {
                address: config.listen,
                source,
            })?;
        Self::with_listener(config, http).await
    }

    /// Prepare the edge on listeners bound elsewhere: the daemon creates
    /// the listening sockets and hands them to the edge process (section
    /// 2.3), so the edge can be replaced without refusing connections. TLS
    /// listeners take their certificates from the resolver, which each
    /// snapshot fills.
    pub async fn adopt(
        config: Config,
        inherited: Vec<Inherited>,
        resolver: Arc<tls::Resolver>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !inherited.is_empty(),
            "the edge needs at least one listener"
        );
        let tls_config = tls::server_config_with_resolver(resolver)?;
        let mut listeners = Vec::new();

        for inherited in inherited {
            inherited.listener.set_nonblocking(true)?;
            let listener = TcpListener::from_std(inherited.listener)?;
            let acceptor = inherited
                .tls
                .then(|| TlsAcceptor::from(Arc::clone(&tls_config)));
            listeners.push((listener, acceptor));
        }

        Self::with_listeners(config, listeners).await
    }

    async fn with_listener(config: Config, http: TcpListener) -> anyhow::Result<Self> {
        Self::with_listeners(config, vec![(http, None)]).await
    }

    async fn with_listeners(
        config: Config,
        mut listeners: Vec<(TcpListener, Option<TlsAcceptor>)>,
    ) -> anyhow::Result<Self> {
        let local = listeners[0].0.local_addr()?;
        let mut redirect_to = None;

        // Every listener is bound before the workers are connected, so a
        // client that finds one port open finds them all open.

        if let Some(secure) = config.https {
            let listener = TcpListener::bind(secure.listen)
                .await
                .map_err(|source| BindError {
                    address: secure.listen,
                    source,
                })?;

            if secure.only {
                redirect_to = Some(listener.local_addr()?);
            }

            listeners.push((listener, Some(TlsAcceptor::from(secure.config))));
        }

        let pools = Arc::new(Pools {
            current: RwLock::new(Arc::new(
                routing::Routing::single(
                    routing::AppPools::connect(
                        &config.app,
                        &config.release,
                        &config.sockets,
                        &config.groups,
                        &config.mounts,
                    )
                    .await?,
                )
                .with_compression(config.compression.clone()),
            )),
            compression: config.compression.clone(),
        });
        let access_log = Arc::new(access::AccessLog::new(config.access_log)?);

        let mut bound = Vec::with_capacity(listeners.len());

        for (listener, acceptor) in listeners {
            let address = listener.local_addr()?;
            bound.push((listener, address, acceptor));
        }

        let https_port = bound
            .iter()
            .find(|(_, _, acceptor)| acceptor.is_some())
            .map(|(_, address, _)| address.port());

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
            .max_header_list_size(u32::try_from(limits.max_header_bytes).unwrap_or(u32::MAX))
            // RFC 8441: WebSockets over HTTP/2 arrive as extended CONNECT.
            .enable_connect_protocol();
        Ok(Self {
            listeners: bound,
            local,
            shared: Arc::new(Shared {
                pools,
                request_ids: errors::RequestIds::new(),
                protocols,
                development: config.development,
                access_log,
                limits,
                redirect_to,
                https_port,
                draining: tokio::sync::watch::channel(false).0,
                connections: Arc::new(()),
            }),
        })
    }

    /// A handle that stops this edge accepting and drains its connections.
    pub fn drainer(&self) -> Drainer {
        Drainer {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The address the first listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The address of the first TLS listener, when there is one.
    pub fn https_addr(&self) -> Option<SocketAddr> {
        self.listeners
            .iter()
            .find(|(_, _, acceptor)| acceptor.is_some())
            .map(|(_, address, _)| *address)
    }

    /// Every listener's address and whether it carries TLS.
    pub fn listeners(&self) -> Vec<(SocketAddr, bool)> {
        self.listeners
            .iter()
            .map(|(_, address, acceptor)| (*address, acceptor.is_some()))
            .collect()
    }

    /// The access log, for the dashboard and the CLI.
    pub fn access_log(&self) -> Arc<access::AccessLog> {
        Arc::clone(&self.shared.access_log)
    }

    /// The worker pools, for switching releases while running.
    pub fn pools(&self) -> Arc<Pools> {
        Arc::clone(&self.shared.pools)
    }

    /// Accept connections on every listener until one fails or the edge
    /// is drained.
    pub async fn run(self) -> anyhow::Result<()> {
        let accepting = self
            .listeners
            .into_iter()
            .map(|(listener, address, acceptor)| {
                accept(listener, address, acceptor, Arc::clone(&self.shared))
            });

        futures::future::try_join_all(accepting).await.map(|_| ())
    }
}

/// Accept connections on one listener, completing the TLS handshake first
/// when it has an acceptor, and serve each on its own task, until the
/// edge drains: the listener is then dropped, and whatever waits in its
/// backlog is accepted by the process that holds it next.
async fn accept(
    listener: TcpListener,
    local: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    shared: Arc<Shared>,
) -> anyhow::Result<()> {
    let mut draining = shared.draining.subscribe();

    loop {
        if *draining.borrow() {
            return Ok(());
        }

        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = draining.changed() => return Ok(()),
        };
        let _ = stream.set_nodelay(true);
        let shared = Arc::clone(&shared);

        // Counted here, before the task starts, so a drain that begins
        // between the two still waits for this connection.

        let token = Arc::clone(&shared.connections);

        match acceptor.clone() {
            None => {
                tokio::spawn(serve(stream, peer, local, None, shared, token));
            }
            Some(acceptor) => {
                tokio::spawn(async move {
                    // A client that never finishes its handshake is cut off
                    // as one that never finishes its request head would be.

                    let handshake = tokio::time::timeout(
                        shared.limits.header_read_timeout,
                        acceptor.accept(stream),
                    )
                    .await;

                    match handshake {
                        Ok(Ok(stream)) => {
                            let info = tls::info(stream.get_ref().1);
                            serve(stream, peer, local, Some(info), shared, token).await;
                        }
                        Ok(Err(error)) => {
                            tracing::debug!(%peer, %error, "TLS handshake failed");
                        }
                        Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
                    }
                });
            }
        }
    }
}

/// Serve one connection, cleartext or already inside TLS. The token is
/// the connection's count towards a drain, held until it ends.
async fn serve<I>(
    io: I,
    peer: SocketAddr,
    local: SocketAddr,
    tls: Option<pb::TlsInfo>,
    shared: Arc<Shared>,
    token: Arc<()>,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let tls = tls.map(Arc::new);
    let scheme = if tls.is_some() { "https" } else { "http" };
    let redirect_to = shared.redirect_to.filter(|_| tls.is_none());
    let state = Arc::clone(&shared);
    let listener = local.to_string();
    let service = service_fn(move |mut request: http::Request<Incoming>| {
        let shared = Arc::clone(&shared);
        let tls = tls.clone();
        let listener = listener.clone();
        let request_id = shared.request_ids.next();
        async move {
            // Section 4.7: forwarding headers from a trusted proxy say who
            // the client is and how it connected; from anyone else they
            // are removed.

            let routing = shared.pools.current();
            let forwarded = routing.trust().resolve(&peer, request.headers_mut());
            let accept_encoding = request
                .headers()
                .get(http::header::ACCEPT_ENCODING)
                .cloned();
            let method = request.method().clone();

            let scheme: &'static str = match forwarded.scheme.as_deref() {
                Some("https") => "https",
                Some("http") => "http",
                _ => scheme,
            };

            let secure = tls.is_some() || scheme == "https";
            let redirect_to = redirect_to.filter(|_| !secure);

            let mut context = link::Context {
                request_id: &request_id,
                peer: forwarded.client.unwrap_or(peer),
                proxy: forwarded.client.map(|_| peer),
                local,
                scheme,
                tls: tls.as_deref(),
                development: shared.development,
                access_log: Arc::clone(&shared.access_log),
                limits: Arc::clone(&shared.limits),
                server_name: None,
                app: String::new(),
                release: String::new(),
                group: String::new(),
                snapshot: routing.version(),
                mount: String::new(),
                mount_path: String::new(),
                forwarded_host: forwarded.host.clone(),
                forwarded_port: forwarded.port,
                prefix: forwarded.prefix.clone(),
            };

            // Sections 4.3 and 4.5: the host the client asked for and the
            // path select the application, or refuse the request. With no
            // TLS listener there is nowhere to redirect to, so a host's
            // https_redirect is ignored.

            let host = forwarded.host.clone().or_else(|| {
                request
                    .headers()
                    .get(HOST)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
                    .or_else(|| request.uri().host().map(str::to_owned))
            });
            let decision = routing.select(
                host.as_deref(),
                request.uri().path(),
                &listener,
                secure || shared.https_port.is_none(),
            );

            let response = match (redirect_to, decision) {
                (Some(to), _) => link::redirect_to_https(request, &context, to),
                (
                    None,
                    routing::Decision::Serve {
                        selection,
                        server_name,
                    },
                ) => {
                    context.server_name = server_name;
                    context.app = selection.app;
                    context.release = selection.release;
                    context.group = selection.group;
                    context.mount = selection.mount;
                    context.mount_path = selection.script_name;
                    link::handle(request, selection.pool, &context).await
                }
                (None, routing::Decision::Redirect) => link::redirect_to_https(
                    request,
                    &context,
                    SocketAddr::new(local.ip(), shared.https_port.unwrap_or(443)),
                ),
                (None, routing::Decision::Misdirected) => link::reject(
                    request,
                    &context,
                    errors::Problem::new(
                        StatusCode::MISDIRECTED_REQUEST,
                        "misdirected",
                        "No virtual host answers to that name on this listener.",
                    ),
                ),
                (
                    None,
                    routing::Decision::Static {
                        route,
                        prefix,
                        fallback,
                        server_name,
                    },
                ) => {
                    // Section 4.9: the file, or the fallback application
                    // when no file answers, or a 404 or 405.

                    context.server_name = server_name;
                    let path = request.uri().path().to_owned();
                    let within = path
                        .strip_prefix(prefix.trim_end_matches('/'))
                        .unwrap_or("")
                        .to_owned();
                    let target = request
                        .uri()
                        .path_and_query()
                        .map_or_else(|| path.clone(), |target| target.to_string());
                    let outcome = static_files::serve(
                        &route,
                        request.method(),
                        &within,
                        &target,
                        request.headers(),
                        routing.compression(),
                    )
                    .await;

                    match (outcome, fallback) {
                        (
                            static_files::Outcome::Served {
                                response,
                                status,
                                reason,
                                bytes,
                            },
                            _,
                        ) => {
                            let (parts, _body) = request.into_parts();
                            link::record(&context, &parts, status, reason, bytes);
                            response
                        }
                        (_, Some(selection)) => {
                            context.app = selection.app.clone();
                            context.release = selection.release.clone();
                            context.group = selection.group.clone();
                            context.mount = selection.mount.clone();
                            context.mount_path = selection.script_name.clone();
                            link::handle(request, selection.pool, &context).await
                        }
                        (static_files::Outcome::MethodNotAllowed, None) => {
                            let (parts, _body) = request.into_parts();
                            link::record(&context, &parts, 405, "method_not_allowed", 0);
                            static_files::method_not_allowed()
                        }
                        (static_files::Outcome::Fallback, None) => link::reject(
                            request,
                            &context,
                            errors::Problem::new(
                                StatusCode::NOT_FOUND,
                                "not_found",
                                "No file answers to that path.",
                            ),
                        ),
                    }
                }
                (None, routing::Decision::NoRoute) => link::reject(
                    request,
                    &context,
                    errors::Problem::new(
                        StatusCode::NOT_FOUND,
                        "no_route",
                        "The virtual host has no route for this path.",
                    ),
                ),
            };

            // Section 4.17: compressed for the client when the policy and
            // the response allow it.

            let mut response = compression::apply(
                routing.compression(),
                &method,
                accept_encoding.as_ref(),
                response,
            );

            // Section 4.19: every response names its request.

            if let Ok(value) = HeaderValue::from_str(&request_id) {
                response
                    .headers_mut()
                    .insert(errors::X_REQUEST_ID.clone(), value);
            }

            Ok::<_, Infallible>(response)
        }
    });

    // With upgrades, so a 101 hands the connection over for WebSockets.
    // A drain tells the connection to finish: HTTP/1.1 closes once the
    // response in flight is done, or at once when idle, and HTTP/2 sends
    // GOAWAY and lets its streams complete.

    let connection = state
        .protocols
        .serve_connection_with_upgrades(TokioIo::new(io), service);
    let mut connection = std::pin::pin!(connection);
    let mut draining = state.draining.subscribe();

    let ended = if *draining.borrow_and_update() {
        connection.as_mut().graceful_shutdown();
        connection.await
    } else {
        tokio::select! {
            ended = connection.as_mut() => ended,
            _ = draining.changed() => {
                connection.as_mut().graceful_shutdown();
                connection.await
            }
        }
    };

    if let Err(error) = ended {
        tracing::debug!(%peer, %error, "connection ended with an error");
    }

    drop(token);
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
    context: &link::Context<'_>,
) -> pb::RequestHead {
    let (peer, local) = (context.peer, context.local);
    let content_length = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let named_in_connection = connection_options(&parts.headers);

    // HTTP/2 carries the host in :authority rather than a Host header, and
    // applications expect HTTP_HOST either way; a host forwarded by a
    // trusted proxy replaces what the proxy itself asked for.

    let forwarded_host = context
        .forwarded_host
        .as_deref()
        .map(|host| Bytes::copy_from_slice(host.as_bytes()));

    let authority = match (parts.headers.get(HOST), parts.uri.authority()) {
        (None, Some(authority)) => Some(Bytes::copy_from_slice(authority.as_str().as_bytes())),
        _ => None,
    };

    let host = forwarded_host.or(authority).map(|value| pb::Header {
        name: Bytes::from_static(b"host"),
        value,
    });
    let replaces_host = context.forwarded_host.is_some();

    // A prefix the proxy stripped is put back, so SCRIPT_NAME and
    // PATH_INFO (ASGI root_path and path) come out as the client sent
    // them; the mount's path joins it as SCRIPT_NAME (section 6.7).

    let raw_path = match &context.prefix {
        Some(prefix) => Bytes::from(format!("{prefix}{}", parts.uri.path())),
        None => Bytes::copy_from_slice(parts.uri.path().as_bytes()),
    };

    let script_name = format!(
        "{}{}",
        context.prefix.as_deref().unwrap_or_default(),
        context.mount_path
    );

    pb::RequestHead {
        protocol_version: pb::PROTOCOL_VERSION.to_owned(),
        request_id: context.request_id.to_owned(),
        method: parts.method.as_str().to_owned(),
        scheme: context.scheme.to_owned(),
        raw_path,
        script_name,
        mount: context.mount.clone(),
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

                if replaces_host && name == "host" {
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
            port: u32::from(context.forwarded_port.unwrap_or_else(|| local.port())),
        }),
        proxy: context.proxy.map(|proxy| pb::Peer {
            address: proxy.ip().to_string(),
            port: u32::from(proxy.port()),
        }),
        tls: context.tls.cloned(),
        content_length,
        http_version: format!("{:?}", parts.version),
        expect_continue: expects_continue(parts) && content_length != Some(0),
        server_name: context.server_name.clone().unwrap_or_default(),
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

/// Whether the HSTS warning of section 13.1 has been given this run.
static HSTS_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// In development, warn once when the application sends
/// Strict-Transport-Security: browsers apply it to every server on that
/// host name, so on localhost it reaches every other local server too.
pub(crate) fn warn_about_hsts(headers: &http::HeaderMap, host: &str, target: &str) {
    if headers.contains_key(http::header::STRICT_TRANSPORT_SECURITY)
        && !HSTS_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        tracing::warn!(
            "the application sent Strict-Transport-Security on {target}; browsers will apply it \
             to every server on {host}, so leave HSTS to production (section 13.1)"
        );
    }
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
