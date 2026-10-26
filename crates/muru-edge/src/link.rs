//! The edge side of the worker link: a pool of persistent connections per
//! worker, each carrying one request at a time (design specification
//! section 5).
//!
//! A worker's pool is bounded by its slot count, so the number of requests
//! in flight at a worker never exceeds what it can run. A request takes a
//! connection, writes a REQUEST frame and then BODY frames, reads RESPONSE
//! and BODY frames back, and returns the connection to the pool once both
//! directions have ended. When the response ends before the request body
//! has been sent, or the client goes away, the edge sends ABORT instead of
//! the rest of the body, so the connection is reusable at once. A connection
//! that fails is dropped and replaced on the next use.
//!
//! Bodies with a declared length stream through; a body without one is
//! read in full before a connection is taken, so the application sees
//! CONTENT_LENGTH (section 4.6). A client waiting on Expect: 100-continue
//! is told to send a streamed body only once the worker's CONTINUE frame
//! says the application is reading it.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use bytes::{Bytes, BytesMut};
use http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::body::{Body, Frame, Incoming};
use muru_proto::descriptors::FdReader;
use muru_proto::frame::{self, flag, kind};
use muru_proto::worker::v1 as pb;
use prost::Message;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::access::{AccessLog, Record};
use crate::errors::{self, Negotiation, Problem};
use crate::routing::{RedirectFlags, Sendfile};
use crate::{BoxError, Limits, ResponseBody, close_after, request_head};

/// After the response has ended, this much of an unread request body is
/// read and discarded so the client connection can be kept alive; beyond
/// it, or after the delay, the connection is closed instead (section 4.6).
const DISCARD_LIMIT: usize = 1024 * 1024;
const DISCARD_DELAY: Duration = Duration::from_secs(2);

/// The largest request body accepted by default (max_body_size in section 4.6, with
/// the same default as mod_wsgi-express). A declared length beyond it is
/// refused before anything is read; an undeclared one as soon as it is
/// exceeded.
pub const DEFAULT_MAX_BODY_SIZE: u64 = 10 * 1024 * 1024;

pub(crate) type Reader = BufReader<FdReader>;
pub(crate) type Writer = OwnedWriteHalf;

/// One idle connection to a worker.
pub(crate) struct Conn {
    pub(crate) reader: Reader,
    pub(crate) writer: Writer,
}

struct WorkerSlots {
    path: PathBuf,
    /// What the worker called itself in HELLO (wsgi or asgi), informational.
    worker_type: String,
    /// Whether it accepts websocket scopes (section 5.6).
    websockets: bool,
    /// Requests the worker takes at once, from its HELLO: its threads for
    /// a WSGI worker, its event loop concurrency for an ASGI one.
    slots: usize,
    semaphore: Arc<Semaphore>,
    idle: Mutex<Vec<Conn>>,
}

/// What a worker's pool looks like right now.
#[derive(Debug, Clone)]
pub struct WorkerStatus {
    /// The worker's index.
    pub index: usize,
    /// Its link socket.
    pub path: PathBuf,
    /// What it called itself in HELLO: wsgi or asgi.
    pub worker_type: String,
    /// Requests it can handle at once: threads, or the loop's concurrency.
    pub slots: usize,
    /// Requests it is handling now.
    pub in_flight: usize,
}

/// Connection pools for a set of workers.
pub struct LinkPool {
    workers: Vec<WorkerSlots>,
    next: AtomicUsize,
    /// Requests waiting for a slot, for the metrics.
    waiting: AtomicUsize,
    /// The gRPC methods the workers' mounts registered, from HELLO
    /// (section 6.13).
    grpc: Vec<pb::GrpcMethod>,
}

/// One request's place in the queue for a slot, given back when it gets
/// one or gives up.
struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A connection taken from the pool for one request.
pub(crate) struct Lease {
    pub(crate) conn: Conn,
    pub(crate) permit: OwnedSemaphorePermit,
    pub(crate) worker: usize,
    /// Whether the worker accepts websocket scopes.
    websockets: bool,
}

impl LinkPool {
    /// Connect once to every worker socket and size its pool from what its
    /// HELLO declares (section 5.1): the edge acts on the capabilities, not
    /// on the kind of worker. The first connection stays in the pool.
    pub async fn connect(sockets: &[PathBuf]) -> anyhow::Result<Self> {
        let mut workers = Vec::with_capacity(sockets.len());
        let mut grpc = Vec::new();

        for path in sockets {
            let (conn, hello) = connect(path)
                .await
                .with_context(|| format!("connecting to the worker at {}", path.display()))?;

            // Every worker of a release registers the same methods.

            if grpc.is_empty() {
                grpc = hello.grpc_methods.clone();
            }

            let slots = capacity(&hello);
            workers.push(WorkerSlots {
                path: path.clone(),
                worker_type: hello.worker_type,
                websockets: hello.websockets,
                slots,
                semaphore: Arc::new(Semaphore::new(slots)),
                idle: Mutex::new(vec![conn]),
            });
        }

        Ok(Self {
            workers,
            next: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            grpc,
        })
    }

    /// Whether the workers serve gRPC methods (section 6.13).
    pub(crate) fn serves_grpc(&self) -> bool {
        !self.grpc.is_empty()
    }

    /// The registered gRPC method at `path`, or the whole service it
    /// belongs to when only that was registered.
    pub(crate) fn grpc_method(&self, path: &str) -> Option<&pb::GrpcMethod> {
        self.grpc
            .iter()
            .find(|method| method.path == path)
            .or_else(|| {
                self.grpc.iter().find(|method| {
                    method.path.ends_with('/') && path.starts_with(method.path.as_str())
                })
            })
    }

    /// Whether a gRPC service of this name is registered.
    pub(crate) fn grpc_service(&self, name: &str) -> bool {
        self.grpc.iter().any(|method| {
            method
                .path
                .strip_prefix('/')
                .and_then(|path| path.split_once('/'))
                .is_some_and(|(service, _)| service == name)
        })
    }

    /// The state of every worker's pool, for the dashboard.
    pub fn workers(&self) -> Vec<WorkerStatus> {
        self.workers
            .iter()
            .enumerate()
            .map(|(index, worker)| WorkerStatus {
                index,
                path: worker.path.clone(),
                worker_type: worker.worker_type.clone(),
                slots: worker.slots,
                in_flight: worker.slots - worker.semaphore.available_permits(),
            })
            .collect()
    }

    /// A pool with no workers at all, for an edge that is up before any
    /// release is: every request is answered 503 until a pool replaces it.
    pub fn empty() -> Self {
        Self {
            workers: Vec::new(),
            next: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            grpc: Vec::new(),
        }
    }

    /// Requests waiting for a slot right now.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }

    /// Whether the pool has any workers.
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Requests in flight across every worker.
    pub fn in_flight(&self) -> usize {
        self.workers
            .iter()
            .map(|worker| worker.slots - worker.semaphore.available_permits())
            .sum()
    }

    /// Take a connection from the worker with the most free slots, waiting
    /// for one if every worker is busy.
    pub(crate) async fn acquire(&self) -> io::Result<Lease> {
        if self.workers.is_empty() {
            return Err(io::Error::other("no release is serving"));
        }

        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let mut best = start;
        let mut best_free = self.workers[start].semaphore.available_permits();

        for offset in 1..self.workers.len() {
            let index = (start + offset) % self.workers.len();
            let free = self.workers[index].semaphore.available_permits();

            if free > best_free {
                best = index;
                best_free = free;
            }
        }

        let worker = &self.workers[best];
        self.waiting.fetch_add(1, Ordering::Relaxed);
        let waiting = Waiting(&self.waiting);
        let permit = Arc::clone(&worker.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("pool closed"))?;
        drop(waiting);
        let idle = worker.idle.lock().expect("pool poisoned").pop();

        let conn = match idle {
            Some(conn) => conn,
            None => connect(&worker.path).await?.0,
        };

        Ok(Lease {
            conn,
            permit,
            worker: best,
            websockets: worker.websockets,
        })
    }

    fn put_back(&self, worker: usize, conn: Conn) {
        self.workers[worker]
            .idle
            .lock()
            .expect("pool poisoned")
            .push(conn);
    }
}

/// How many requests a worker takes at once: on its threads when it has
/// them, otherwise on its event loop.
fn capacity(hello: &pb::Hello) -> usize {
    let declared = if hello.slots > 0 {
        hello.slots
    } else {
        hello.async_slots
    };

    usize::try_from(declared).unwrap_or(usize::MAX).max(1)
}

async fn connect(path: &PathBuf) -> io::Result<(Conn, pb::Hello)> {
    let stream = UnixStream::connect(path).await?;
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::with_capacity(32 * 1024, FdReader::new(read_half));
    let mut writer = write_half;
    let hello = pb::Hello {
        protocol_version: pb::PROTOCOL_VERSION.to_owned(),
        ..Default::default()
    };
    frame::write(&mut writer, kind::HELLO, 0, hello.encode_to_vec().into()).await?;

    let hello = match frame::read(&mut reader).await? {
        Some(reply) if reply.kind == kind::HELLO => pb::Hello::decode(reply.payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the worker did not answer HELLO",
            ));
        }
    };

    Ok((Conn { reader, writer }, hello))
}

/// Collects the two halves of a leased connection as each direction
/// finishes, and returns the connection to the pool when both are back
/// intact. A half reported as broken closes the connection instead.
pub(crate) struct Return {
    pool: Arc<LinkPool>,
    worker: usize,
    state: Mutex<ReturnState>,
    /// Signalled when the request direction should stop: the response has
    /// ended, or failed.
    pub(crate) abort: Notify,
    /// Signalled when the worker has sent CONTINUE: the application is
    /// reading the body, so a waiting client may be told to send it.
    proceed: Notify,
    proceeded: AtomicBool,
    /// Set when the client stopped sending its body for too long.
    client_stalled: AtomicBool,
    /// Set once the response direction has ended, intact or not.
    ended: AtomicBool,
    /// The request's stored uploads (section 4.11), dropped with it.
    uploads: Mutex<Option<crate::uploads::Received>>,
}

struct ReturnState {
    reader: Option<Reader>,
    writer: Option<Writer>,
    broken: bool,
    reported: u8,
    permit: Option<OwnedSemaphorePermit>,
}

impl Return {
    pub(crate) fn new(
        pool: Arc<LinkPool>,
        worker: usize,
        permit: OwnedSemaphorePermit,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            worker,
            state: Mutex::new(ReturnState {
                reader: None,
                writer: None,
                broken: false,
                reported: 0,
                permit: Some(permit),
            }),
            abort: Notify::new(),
            proceed: Notify::new(),
            proceeded: AtomicBool::new(false),
            client_stalled: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            uploads: Mutex::new(None),
        })
    }

    /// Keep the request's stored uploads until the request ends.
    fn hold(&self, uploads: crate::uploads::Received) {
        *self.uploads.lock().expect("uploads poisoned") = Some(uploads);
    }

    /// Tell the body pump, if it is still running, to send ABORT and stop.
    pub(crate) fn abort_request(&self) {
        self.abort.notify_one();
    }

    /// Tell the body pump, if it is waiting, that the application is reading.
    fn continue_request(&self) {
        self.proceeded.store(true, Ordering::Relaxed);
        self.proceed.notify_one();
    }

    pub(crate) fn give_reader(&self, reader: Option<Reader>) {
        self.ended.store(true, Ordering::Relaxed);
        let mut state = self.state.lock().expect("return state poisoned");
        state.reader = reader;
        self.report(&mut state);
    }

    /// Take the writer back for a WebSocket relay, which holds both halves
    /// until the socket ends. The earlier report is undone.
    pub(crate) fn take_writer(&self) -> Option<Writer> {
        let mut state = self.state.lock().expect("return state poisoned");
        let writer = state.writer.take();

        if writer.is_some() {
            state.reported = state.reported.saturating_sub(1);
        }

        writer
    }

    pub(crate) fn give_writer(&self, writer: Option<Writer>) {
        let mut state = self.state.lock().expect("return state poisoned");
        state.writer = writer;
        self.report(&mut state);
    }

    fn report(&self, state: &mut ReturnState) {
        state.reported += 1;

        if state.reader.is_none() && state.writer.is_none() {
            state.broken = true;
        }

        if state.reported < 2 {
            return;
        }

        let (reader, writer) = (state.reader.take(), state.writer.take());

        if let (Some(reader), Some(writer), false) = (reader, writer, state.broken) {
            self.pool.put_back(self.worker, Conn { reader, writer });
        }

        // Release the slot after the connection is back in the pool, so the
        // next request finds it there.

        state.permit.take();
    }
}

impl Drop for Return {
    /// Dropped with the response still in progress means the client went
    /// away before the worker answered. Section 5.5 asks for an ABORT so
    /// the application learns of it; the connection is then closed rather
    /// than drained for reuse, which is left for the daemon's edge.
    fn drop(&mut self) {
        if self.ended.load(Ordering::Relaxed) {
            return;
        }

        // Drop runs inside the runtime when a request's task is dropped;
        // without one there is nothing to write from.

        let writer = self
            .state
            .lock()
            .expect("return state poisoned")
            .writer
            .take();

        if let (Some(mut writer), Ok(runtime)) = (writer, tokio::runtime::Handle::try_current()) {
            runtime.spawn(async move {
                let payload = pb::Abort::default().encode_to_vec();
                let _ = frame::write(&mut writer, kind::ABORT, 0, payload.into()).await;
            });
        }
    }
}

/// What the edge knows about a request before it is handled.
pub struct Context<'a> {
    /// The request identifier (section 4.19).
    pub request_id: &'a str,
    /// The client's address: the peer, or what a trusted proxy forwarded.
    pub peer: SocketAddr,
    /// The peer that connected, when it was a trusted proxy (section 4.7).
    pub proxy: Option<SocketAddr>,
    /// The listener's address.
    pub local: SocketAddr,
    /// http, or https when the connection is inside TLS.
    pub scheme: &'static str,
    /// What TLS negotiated, when the connection is inside it.
    pub tls: Option<&'a pb::TlsInfo>,
    /// Whether error pages show tracebacks.
    pub development: bool,
    /// Where the request's record goes once it is complete.
    pub access_log: Arc<AccessLog>,
    /// The timeouts in force.
    pub limits: Arc<Limits>,
    /// The virtual host name the application sees as SERVER_NAME, when a
    /// host matched (section 4.3).
    pub server_name: Option<String>,
    /// The application the request goes to, for the access log; empty
    /// before routing, or when nothing names it.
    pub app: String,
    /// The release, for the access log; empty when nothing names it.
    pub release: String,
    /// The worker group, for the access log; empty before routing.
    pub group: String,
    /// The routing snapshot's version, for the access log.
    pub snapshot: u64,
    /// The host a trusted proxy forwarded, replacing the Host header.
    pub forwarded_host: Option<String>,
    /// The port a trusted proxy forwarded, for SERVER_PORT.
    pub forwarded_port: Option<u16>,
    /// The path prefix a trusted proxy stripped, for SCRIPT_NAME.
    pub prefix: Option<String>,
    /// The mount the request goes to, empty for the default (section 6.7).
    pub mount: String,
    /// The mount's path, joined to SCRIPT_NAME.
    pub mount_path: String,
    /// The file hints by path the mount, upstream or server may give
    /// (section 4.9).
    pub sendfile: Arc<Sendfile>,
    /// How the request arrived by internal redirect (section 4.10), for
    /// the request head; none for a client's own request.
    pub internal: Option<Internal>,
    /// When the client's request arrived: the request deadline bounds
    /// the whole redirect chain from here.
    pub arrived: Instant,
    /// The target the client asked for, on a redirect hop, so the one
    /// access log entry for the chain names it.
    pub client_target: Option<String>,
    /// The targets the request has been redirected to so far.
    pub hops: Vec<String>,
    /// The error pages and override in force (section 4.12).
    pub errors: errors::Chain,
    /// This hop is a sub-request the edge makes for itself: for an error
    /// page (section 4.12) or an authorisation decision (section 4.16).
    /// The value is the handler's budget, queueing included; its own
    /// errors are never replaced, and it is logged as a hop of the
    /// request it serves.
    pub handler: Option<Duration>,
    /// How the route's uploads are received (section 4.11); none for a
    /// route without uploads and for any request but a client's own.
    pub upload: Option<Arc<crate::uploads::Policy>>,
    /// Where uploads are stored.
    pub spool: Option<Arc<crate::uploads::Spool>>,
    /// The client's class (section 4.14).
    pub client_class: Arc<str>,
    /// The route's site mode (section 4.15), for the application.
    pub mode: Option<Arc<str>>,
}

impl<'a> Context<'a> {
    /// The context of a sub-request the edge makes for itself (section
    /// 4.16), under this request: the same client and connection, the
    /// hops so far, and no application until the handler is selected.
    pub(crate) fn sub_request(
        &self,
        request_id: &'a str,
        budget: Duration,
        internal: Internal,
        target: String,
    ) -> Context<'a> {
        Context {
            request_id,
            peer: self.peer,
            proxy: self.proxy,
            local: self.local,
            scheme: self.scheme,
            tls: self.tls,
            development: self.development,
            access_log: Arc::clone(&self.access_log),
            limits: Arc::clone(&self.limits),
            server_name: self.server_name.clone(),
            app: String::new(),
            release: String::new(),
            group: String::new(),
            snapshot: self.snapshot,
            mount: String::new(),
            mount_path: String::new(),
            sendfile: Arc::default(),
            forwarded_host: self.forwarded_host.clone(),
            forwarded_port: self.forwarded_port,
            prefix: self.prefix.clone(),
            internal: Some(internal),
            arrived: self.arrived,
            client_target: Some(target),
            hops: self.hops.clone(),
            errors: errors::Chain::default(),
            handler: Some(budget),
            upload: None,
            spool: self.spool.clone(),
            client_class: Arc::clone(&self.client_class),
            mode: None,
        }
    }
}

/// How long an application gets to produce an error page (section 4.12).
pub const HANDLER_TIMEOUT: Duration = Duration::from_secs(2);

/// What a redirected sub-request's application learns about the request
/// it stands in for (section 4.10): REDIRECT_URL, REDIRECT_QUERY_STRING
/// and REDIRECT_STATUS, the trusted headers the redirecting application
/// added, and which hop this is.
#[derive(Debug, Clone)]
pub struct Internal {
    /// The path the client asked for.
    pub redirect_url: String,
    /// Its query string.
    pub redirect_query_string: String,
    /// The status the redirecting application answered.
    pub redirect_status: u16,
    /// Headers added with X-Muru-Redirect-Header-NAME, by name.
    pub headers: Vec<(String, Bytes)>,
    /// 1 for the first redirect.
    pub hop: u32,
    /// For an error page sub-request (section 4.12), the reason of the
    /// error, as muru.error_reason; empty for a redirect.
    pub error_reason: String,
    /// For an authentication sub-request (section 4.16), the request it
    /// decides about; the application then sees muru.auth_request and
    /// the original request rather than REDIRECT_* keys.
    pub original: Option<Original>,
}

/// The request an authentication sub-request decides about (section
/// 4.16), as muru.original_method, muru.original_uri and
/// muru.original_host.
#[derive(Debug, Clone)]
pub struct Original {
    /// Its method.
    pub method: String,
    /// Its path and query.
    pub uri: String,
    /// The host it asked for.
    pub host: String,
}

/// What handling a request produced: a response for the client, or an
/// internal redirect to route afresh (section 4.10).
pub enum Handled {
    /// The response to send.
    Response(Response<ResponseBody>),
    /// The application redirected internally.
    Redirect(Box<Redirect>),
}

/// An internal redirect an application answered with (section 4.10).
#[derive(Debug)]
pub struct Redirect {
    /// The path, with its query, to route the sub-request to.
    pub target: String,
    /// Another configured host to route it under, when named.
    pub host: Option<String>,
    /// The status the application answered with, for REDIRECT_STATUS.
    pub status: u16,
    /// Keep the method, and the body when it was buffered, rather than
    /// turning the sub-request into a GET.
    pub preserve_method: bool,
    /// Trusted headers for the target, from X-Muru-Redirect-Header-NAME.
    pub headers: Vec<(String, Bytes)>,
    /// The redirecting response's headers that carry over to the final
    /// response: Content-Disposition, Cache-Control, Expires, Set-Cookie,
    /// Vary and a non-empty Content-Type.
    pub merged: HeaderMap,
    /// The buffered request body, for a preserved method.
    pub body: Option<Bytes>,
}

/// The most hops an internal redirect chain may take.
pub const MAX_REDIRECT_HOPS: usize = 5;

/// The host the client asked for, without its port, from the Host header
/// or the request target.
pub(crate) fn requested_host(parts: &http::request::Parts) -> String {
    let header = parts
        .headers
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| parts.uri.host())
        .unwrap_or("");
    crate::routing::host_only(header).to_owned()
}

/// The access log record of a request in progress.
pub(crate) struct Recording {
    log: Arc<AccessLog>,
    /// Logged as a hop rather than a line of its own (section 4.12).
    silent: bool,
    pub(crate) record: Record,
    started: Instant,
    /// Counted as in flight until the recording is dropped.
    _in_flight: crate::access::InFlight,
}

/// Record a response the edge produced itself, such as a static file,
/// in the access log.
pub(crate) fn record(
    context: &Context<'_>,
    parts: &http::request::Parts,
    status: u16,
    reason: &str,
    bytes: u64,
) {
    Recording::new(context, parts).complete(status, reason, bytes);
}

impl Recording {
    pub(crate) fn new(context: &Context<'_>, parts: &http::request::Parts) -> Self {
        Self {
            log: Arc::clone(&context.access_log),
            silent: context.handler.is_some(),
            _in_flight: context.access_log.begin(&context.app, &context.release),
            record: Record {
                started: SystemTime::now(),
                request_id: context.request_id.to_owned(),
                client: context.peer.ip(),
                app: context.app.clone(),
                release: context.release.clone(),
                host: context
                    .server_name
                    .clone()
                    .unwrap_or_else(|| requested_host(parts)),
                group: context.group.clone(),
                mount: if context.mount.is_empty() {
                    crate::routing::DEFAULT_GROUP.to_owned()
                } else {
                    context.mount.clone()
                },
                snapshot: context.snapshot,
                method: parts.method.as_str().to_owned(),
                target: context.client_target.clone().unwrap_or_else(|| {
                    parts
                        .uri
                        .path_and_query()
                        .map_or_else(|| parts.uri.path().to_owned(), |target| target.to_string())
                }),
                version: format!("{:?}", parts.version),
                status: 0,
                reason: String::new(),
                bytes: 0,
                worker: None,
                queue: Duration::ZERO,
                application: Duration::ZERO,
                total: Duration::ZERO,
                redirects: context.hops.clone(),
                client_class: context.client_class.to_string(),
            },
            started: Instant::now(),
        }
    }

    /// Section 4.10: the request went on by internal redirect. The hop
    /// counts in the metrics of the application that answered it; the
    /// line is written once the chain ends.
    pub(crate) fn hop(mut self, status: u16) {
        self.record.status = status;
        self.record.total = self.started.elapsed();
        self.log.record_hop(&self.record);
    }

    pub(crate) fn complete(mut self, status: u16, reason: &str, bytes: u64) {
        self.record.status = status;
        self.record.reason = reason.to_owned();
        self.record.bytes = bytes;
        self.record.total = self.started.elapsed();

        // Section 4.12: an error page sub-request counts in the metrics
        // of the application that served it, while the line is the
        // failed request's own.

        if self.silent {
            self.log.record_hop(&self.record);
        } else {
            self.log.record(self.record);
        }
    }
}

/// Answer a plain HTTP request with a redirect to the HTTPS listener
/// (--https-only, section 13.1). 307 keeps the method and is never cached,
/// so a later run without TLS on the same port is unaffected.
/// Answer a request the routing tables refuse (421 or 404), recording it
/// in the access log like any other edge-generated response.
pub(crate) fn reject(
    parts: &http::request::Parts,
    context: &Context<'_>,
    problem: Problem,
) -> Response<ResponseBody> {
    let recording = Recording::new(context, parts);
    failing(context, parts, Some(recording)).respond(problem)
}

/// How a request's failures are answered, with `recording` completed by
/// the first failure answered.
pub(crate) fn failing<'a>(
    context: &'a Context<'_>,
    parts: &http::request::Parts,
    recording: Option<Recording>,
) -> Failing<'a> {
    Failing {
        request_id: context.request_id,
        max_body_size: context.limits.max_body_size,
        negotiation: Negotiation::from_headers(&parts.headers, context.development),
        version: parts.version,
        recording: Mutex::new(recording),
        errors: &context.errors,
        host: requested_host(parts),
        path: parts.uri.path().to_owned(),
    }
}

pub(crate) fn redirect_to_https(
    parts: &http::request::Parts,
    context: &Context<'_>,
    to: SocketAddr,
) -> Response<ResponseBody> {
    let recording = Recording::new(context, parts);
    let host = parts
        .headers
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(crate::routing::host_only)
        .or_else(|| parts.uri.host().map(str::to_owned))
        .unwrap_or_else(|| match to.ip() {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        });

    let port = if to.port() == 443 {
        String::new()
    } else {
        format!(":{}", to.port())
    };
    let location = format!("https://{host}{port}{}", recording.record.target);
    let empty = Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync();
    let mut response = Response::new(empty);
    *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    let headers = response.headers_mut();

    if let Ok(value) = http::HeaderValue::from_str(&location) {
        headers.insert(http::header::LOCATION, value);
    }

    headers.insert(http::header::CONTENT_LENGTH, http::HeaderValue::from(0));
    headers.insert(http::header::SERVER, http::HeaderValue::from_static("muru"));
    recording.complete(StatusCode::TEMPORARY_REDIRECT.as_u16(), "", 0);
    response
}

/// Send a request to a worker of the pool and relay its response. `body`
/// is the client's body, or none for a redirected sub-request, whose
/// preserved body, if any, is `buffered`; `flags` say which response
/// forms from the mount count as internal redirects (section 4.10).
pub async fn handle(
    parts: &mut http::request::Parts,
    body: Option<Incoming>,
    buffered: Option<Bytes>,
    pool: Arc<LinkPool>,
    context: &Context<'_>,
    flags: RedirectFlags,
) -> Handled {
    // Section 6.13: a client's gRPC call to a release that registered
    // gRPC methods takes its own path.

    if context.internal.is_none()
        && context.handler.is_none()
        && pool.serves_grpc()
        && let Some(protocol) = crate::grpc::Protocol::of(&parts.headers)
    {
        return Handled::Response(crate::grpc::handle(parts, body, pool, context, protocol).await);
    }

    // Section 4.11: an upload route with precheck asks the application
    // with the headers alone before the body is read, and anything but a
    // 2xx is the answer; the client is told to send its body only after.

    if let Some(policy) = &context.upload
        && policy.precheck
        && buffered.is_none()
        && body.as_ref().is_some_and(|body| !body.is_end_stream())
    {
        let mut redirected = None;
        let response = handle_inner(
            parts,
            None,
            None,
            Arc::clone(&pool),
            context,
            flags,
            true,
            &mut redirected,
        )
        .await;

        if let Some(redirect) = redirected {
            return Handled::Redirect(redirect);
        }

        if !response.status().is_success() {
            return Handled::Response(response);
        }
    }

    let mut redirected = None;
    let response = handle_inner(
        parts,
        body,
        buffered,
        pool,
        context,
        flags,
        false,
        &mut redirected,
    )
    .await;

    match redirected {
        Some(redirect) => Handled::Redirect(redirect),
        None => Handled::Response(response),
    }
}

/// `handle` itself: a redirect is reported through `redirected`, with a
/// placeholder response the caller discards. A `precheck` request goes
/// with the headers alone and is logged as a hop when it is allowed.
#[allow(clippy::too_many_arguments)] // two call sites; each argument is a distinct part of the request
async fn handle_inner(
    parts: &mut http::request::Parts,
    mut body: Option<Incoming>,
    buffered: Option<Bytes>,
    pool: Arc<LinkPool>,
    context: &Context<'_>,
    flags: RedirectFlags,
    precheck: bool,
    redirected: &mut Option<Box<Redirect>>,
) -> Response<ResponseBody> {
    let upgrade = crate::websocket::detect(parts);
    let mut head = request_head(parts, context);
    let limits = &context.limits;

    if precheck {
        head.precheck = true;
        head.expect_continue = false;
    }

    // Sections 4.12 and 4.16: an application asked for an error page or
    // an authorisation decision gets the handler's budget out of what is
    // left of the deadline, queueing included.

    let (mut deadline, queue_timeout) = if let Some(budget) = context.handler {
        (
            (context.arrived + limits.request_deadline).min(Instant::now() + budget),
            limits.queue_timeout.min(budget),
        )
    } else {
        (
            context.arrived + limits.request_deadline,
            limits.queue_timeout,
        )
    };
    head.deadline_unix_ms = unix_deadline(deadline);
    let mut recording = Recording::new(context, parts);
    let failing = Failing {
        request_id: context.request_id,
        max_body_size: limits.max_body_size,
        negotiation: Negotiation::from_headers(&parts.headers, context.development),
        version: parts.version,
        recording: Mutex::new(None),
        errors: &context.errors,
        host: requested_host(parts),
        path: parts.uri.path().to_owned(),
    };

    head.max_body_size = limits.max_body_size;
    if head
        .content_length
        .is_some_and(|length| length > limits.max_body_size)
    {
        *failing.recording.lock().expect("recording poisoned") = Some(recording);
        return failing.too_large();
    }

    // Section 4.6: a body without a declared length is read in full first,
    // so that the application sees CONTENT_LENGTH and no worker slot is
    // held while it arrives. Polling the body is also what makes hyper
    // send 100 Continue to a client that is waiting for it. A redirected
    // sub-request brings its body already buffered, or none.

    let mut buffered: Option<Bytes> = buffered;

    if let Some(data) = &buffered {
        head.content_length = Some(data.len() as u64);
        head.body_buffered = true;
    }

    // Section 4.11: an upload route receives the body into the spool
    // first. The application sees the rewritten form, or an empty body
    // for a raw upload, with the files by descriptor, and the request
    // deadline runs from when the upload has arrived.

    let mut received: Option<crate::uploads::Received> = None;

    if !precheck
        && buffered.is_none()
        && let Some(policy) = context.upload.as_ref()
        && let Some(incoming) = body.as_mut()
        && !incoming.is_end_stream()
    {
        let outcome = match context.spool.as_ref() {
            Some(spool) => {
                crate::uploads::receive(
                    policy,
                    spool,
                    &context.app,
                    &parts.headers,
                    incoming,
                    limits.max_body_size,
                    limits.body_idle_timeout,
                )
                .await
            }
            None => Err(crate::uploads::Refusal::SpoolFull(Some(
                std::io::Error::other("the edge has no spool directory"),
            ))),
        };

        match outcome {
            Ok(Some(upload)) => {
                let length = upload.body.len() as u64;
                set_content_length(&mut head, length);
                head.content_length = Some(length);
                head.body_buffered = true;
                head.expect_continue = false;
                head.uploads = upload.refs();
                buffered = Some(upload.body.clone());
                received = Some(upload);
                deadline = Instant::now() + limits.request_deadline;
                head.deadline_unix_ms = unix_deadline(deadline);
            }
            Ok(None) => {}
            Err(crate::uploads::Refusal::BodyTooLarge) => {
                *failing.recording.lock().expect("recording poisoned") = Some(recording);
                return failing.too_large();
            }
            Err(refusal) => {
                match &refusal {
                    crate::uploads::Refusal::SpoolFull(Some(error)) => {
                        tracing::warn!(%error, app = %context.app, "storing an upload failed");
                    }
                    crate::uploads::Refusal::SpoolFull(None) => {
                        tracing::warn!(app = %context.app, "an upload was refused: the spool quota is reached");
                    }
                    crate::uploads::Refusal::Malformed(error) => {
                        tracing::debug!(%error, "a multipart upload could not be parsed");
                    }
                    _ => {}
                }

                let problem = refusal
                    .problem()
                    .expect("only a body over max_body_size has none");
                *failing.recording.lock().expect("recording poisoned") = Some(recording);
                return failing.close(problem);
            }
        }
    }

    if buffered.is_none()
        && let Some(incoming) = body.as_mut()
        && !incoming.is_end_stream()
        && head.content_length.is_none()
    {
        match read_body(incoming, limits.max_body_size).await {
            Ok(data) => {
                head.content_length = Some(data.len() as u64);
                head.body_buffered = true;
                buffered = Some(data);
            }
            Err(BodyError::TooLarge) => {
                *failing.recording.lock().expect("recording poisoned") = Some(recording);
                return failing.too_large();
            }
            Err(BodyError::Read(error)) => {
                tracing::debug!(%error, "reading the request body failed");
                *failing.recording.lock().expect("recording poisoned") = Some(recording);
                return failing.close(Problem::new(
                    StatusCode::BAD_REQUEST,
                    "malformed",
                    "The request body could not be read.",
                ));
            }
        }
    }

    let expect_continue = head.expect_continue && !head.body_buffered;

    let queued = Instant::now();

    let lease = match tokio::time::timeout(queue_timeout, pool.acquire()).await {
        Ok(Ok(lease)) => lease,
        Ok(Err(error)) => {
            tracing::warn!(%error, "no worker connection");
            let mut problem = Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_workers",
                "No worker is available to handle the request.",
            );
            problem.retry_after = Some(1);
            *failing.recording.lock().expect("recording poisoned") = Some(recording);
            return failing.respond(problem);
        }
        Err(_) => {
            // Section 7.6: the queue wait is bounded by queue_timeout.

            let mut problem = Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "queue_full",
                "Every worker stayed busy for the whole queue timeout.",
            );
            problem.retry_after = Some(1);
            *failing.recording.lock().expect("recording poisoned") = Some(recording);
            return failing.respond(problem);
        }
    };

    let Lease {
        conn,
        permit,
        worker,
        websockets,
    } = lease;
    let Conn {
        mut reader,
        mut writer,
    } = conn;
    recording.record.worker = Some(worker);
    recording.record.queue = queued.elapsed();
    let dispatched = Instant::now();

    // Section 5.6: a worker that takes websocket scopes gets the upgrade as
    // one, with no body; any other worker sees an ordinary request.

    let upgrade = upgrade.filter(|_| websockets);

    if upgrade.is_some() {
        head.scope = pb::Scope::Websocket as i32;
    }

    let end = upgrade.is_some()
        || buffered.as_ref().map_or_else(
            || body.as_ref().is_none_or(|body| body.is_end_stream()),
            Bytes::is_empty,
        );
    let kept = buffered.clone();
    let frame_flags = if end { flag::END } else { 0 };
    *failing.recording.lock().expect("recording poisoned") = Some(recording);

    // Section 5.7: stored uploads go with the request head, attached to
    // its first bytes.

    let written = match &received {
        Some(upload) if !upload.files.is_empty() => {
            let encoded = frame::encode(kind::REQUEST, frame_flags, &head.encode_to_vec());
            muru_proto::descriptors::write_with_fds(&mut writer, &encoded, &upload.fds()).await
        }
        _ => {
            frame::write(
                &mut writer,
                kind::REQUEST,
                frame_flags,
                head.encode_to_vec().into(),
            )
            .await
        }
    };

    if let Err(error) = written {
        tracing::warn!(%error, "writing the request head failed");
        drop(permit);
        return failing.worker_crashed("The worker is not available.");
    }

    let back = Return::new(pool, worker, permit);

    // The stored files stay until the request ends: an unnamed one
    // vanishes with its last descriptor, a named one is removed.

    if let Some(upload) = received {
        back.hold(upload);
    }

    match buffered {
        _ if upgrade.is_some() => back.give_writer(Some(writer)),
        Some(data) if !data.is_empty() => {
            match frame::write(&mut writer, kind::BODY, flag::END, data).await {
                Ok(()) => back.give_writer(Some(writer)),
                Err(error) => {
                    tracing::warn!(%error, "writing the request body failed");
                    back.give_writer(None);
                }
            }
        }
        Some(_) => back.give_writer(Some(writer)),
        None if end => back.give_writer(Some(writer)),
        None => match body.take() {
            Some(body) => {
                tokio::spawn(pump_body(
                    body,
                    writer,
                    Arc::clone(&back),
                    expect_continue,
                    limits.body_idle_timeout,
                ));
            }
            None => back.give_writer(Some(writer)),
        },
    }

    let response = loop {
        let next = match tokio::time::timeout_at(deadline.into(), frame::read(&mut reader)).await {
            Ok(next) => next,
            Err(_) => {
                // Section 7.6: the deadline cancels the request. Dropping
                // the connection stops the application's iteration.

                back.give_reader(None);
                back.abort_request();
                return failing.respond(Problem::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "deadline",
                    "The application did not respond within the request deadline.",
                ));
            }
        };

        match next {
            Ok(Some(next)) if next.kind == kind::CONTINUE => back.continue_request(),
            Ok(Some(next)) if next.kind == kind::RESPONSE => {
                if back.client_stalled.load(Ordering::Relaxed) {
                    // The client stopped sending its body; what the worker
                    // produced for a partial request is not delivered.

                    back.give_reader(None);
                    return failing.close(Problem::new(
                        StatusCode::REQUEST_TIMEOUT,
                        "client_timeout",
                        "The request body stopped arriving.",
                    ));
                }

                break next;
            }
            Ok(Some(next)) if next.kind == kind::END => {
                // The application failed before producing a head: the
                // worker says why, and the connection is still good.

                back.give_reader(Some(reader));
                back.abort_request();
                let end = pb::End::decode(next.payload).unwrap_or_default();
                return failing.worker_failed(end);
            }
            Ok(_) => {
                back.give_reader(None);
                return failing.worker_crashed("The worker sent no response head.");
            }
            Err(error) => {
                tracing::warn!(%error, "reading the response head failed");
                back.give_reader(None);
                return failing.worker_crashed("The worker failed.");
            }
        }
    };

    let response_end = response.is_end();
    let Ok(head) = pb::ResponseHead::decode(response.payload) else {
        back.give_reader(None);
        return failing.worker_crashed("The worker sent an invalid response head.");
    };
    let mut recording = failing.recording.lock().expect("recording poisoned").take();

    if let Some(recording) = recording.as_mut() {
        recording.record.application = dispatched.elapsed();
        recording.record.status = u16::try_from(head.status).unwrap_or(500);

        // Section 4.11: a precheck the application allows is a hop of the
        // request that follows it; one it refuses is the answer.

        recording.silent |= precheck && (200..300).contains(&recording.record.status);
    }

    // Section 4.10: an internal redirect is not a response for the client
    // but a request to route afresh. The rest of the worker's response
    // is drained so the connection goes back to the pool, and the hop
    // is counted for the application that answered it.

    if upgrade.is_none()
        && let Some(redirect) = redirect_from(&head, response_end, flags, kept)
    {
        if response_end {
            back.give_reader(Some(reader));
            back.abort_request();
        } else {
            drain_response(reader, &back, limits.response_idle_timeout).await;
        }

        if let Some(recording) = recording.take() {
            recording.hop(u16::try_from(head.status).unwrap_or(500));
        }

        *redirected = Some(Box::new(redirect));
        return failing.respond(Problem::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "redirect",
            "The application redirected internally.",
        ));
    }

    // Section 4.12: the override replaces an error response of the
    // application's own with the error page, before any body byte goes
    // out. The rest of the response is drained as for a redirect.

    if upgrade.is_none()
        && context.handler.is_none()
        && let Some(problem) =
            override_from(&head, response_end, &failing.negotiation, &context.errors)
    {
        if response_end {
            back.give_reader(Some(reader));
            back.abort_request();
        } else {
            drain_response(reader, &back, limits.response_idle_timeout).await;
        }

        *failing.recording.lock().expect("recording poisoned") = recording.take();
        return failing.respond(problem);
    }

    // Section 4.9: a file hint by path replaces the response's body with
    // the file, served by the edge; the rest of the worker's response is
    // drained as for a redirect.

    if upgrade.is_none()
        && let Some(path) = sendfile_from(&head, &context.sendfile)
    {
        if response_end {
            back.give_reader(Some(reader));
            back.abort_request();
        } else {
            drain_response(reader, &back, limits.response_idle_timeout).await;
        }

        return serve_hint(&path, parts, context, &head, &failing, recording).await;
    }

    // The worker accepted the WebSocket: finish the handshake and relay
    // messages for the life of the socket on the leased connection. Any
    // other answer is the response to the upgrade, sent as usual.

    if let Some(upgrade) = upgrade
        && head.status == 101
    {
        let Some(writer) = back.take_writer() else {
            back.give_reader(None);
            return failing.worker_crashed("The worker connection was lost.");
        };
        let response = crate::websocket::accepted_response(&upgrade, head.headers);
        tokio::spawn(crate::websocket::relay(crate::websocket::Relay {
            upgrade,
            reader,
            writer,
            back,
            recording,
            limits: Arc::clone(&context.limits),
        }));
        return response;
    }

    let mut builder = Response::builder().status(crate::status_code(head.status));

    if let Some(headers) = builder.headers_mut() {
        crate::copy_headers(headers, head.headers);

        if context.development {
            let host = parts
                .headers
                .get(http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("this host");
            crate::warn_about_hsts(headers, host, parts.uri.path());
        }
    }

    if !head.reason.is_empty()
        && let Ok(reason) = hyper::ext::ReasonPhrase::try_from(head.reason.into_bytes())
    {
        builder = builder.extension(reason);
    }

    // A client still waiting for 100 Continue may send the body after this
    // response, so the connection cannot be reused (section 4.6).

    let close = expect_continue && !back.proceeded.load(Ordering::Relaxed);

    let response = if response_end {
        back.give_reader(Some(reader));
        back.abort_request();

        if let Some(recording) = recording.take() {
            let status = recording.record.status;
            recording.complete(status, "", 0);
        }

        let empty = Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync();
        builder.body(empty)
    } else {
        let stream = futures::stream::unfold(
            BodyState {
                reader: Some(reader),
                back,
                recording,
                bytes: 0,
                idle_timeout: limits.response_idle_timeout,
                file: None,
            },
            next_chunk,
        );
        builder.body(UnsyncBoxBody::new(StreamBody::new(stream)))
    };

    let response = match response {
        Ok(response) => response,
        Err(_) => return failing.worker_crashed("The worker sent an invalid response head."),
    };

    if close {
        close_after(response, failing.version)
    } else {
        response
    }
}

/// The internal redirect a response head carries, if any (section 4.10):
/// X-Muru-Redirect always, X-Accel-Redirect and the CGI local redirect (a
/// 200 with a Location starting with `/` and no body) when the mount
/// opts in. The target must be an absolute path on this server. `body`
/// is the buffered request body, kept for a preserved method.
pub(crate) fn redirect_from(
    head: &pb::ResponseHead,
    response_end: bool,
    flags: RedirectFlags,
    body: Option<Bytes>,
) -> Option<Redirect> {
    const HEADER_PREFIX: &str = "x-muru-redirect-header-";
    let mut target: Option<String> = None;
    let mut accel: Option<String> = None;
    let mut location: Option<String> = None;
    let mut host = None;
    let mut preserve = false;
    let mut empty_body = response_end;
    let mut headers = Vec::new();
    let mut merged = HeaderMap::new();

    for header in &head.headers {
        let name = String::from_utf8_lossy(&header.name).to_ascii_lowercase();
        let value = String::from_utf8_lossy(&header.value).trim().to_owned();

        match name.as_str() {
            "x-muru-redirect" => target = Some(value),
            "x-accel-redirect" => accel = Some(value),
            "x-muru-redirect-host" => host = Some(value),
            "x-muru-redirect-method" => preserve = value.eq_ignore_ascii_case("preserve"),
            "location" => location = Some(value),
            "content-length" => empty_body |= value == "0",
            "content-disposition" | "cache-control" | "expires" | "set-cookie" | "vary" => {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_bytes(&header.value),
                ) {
                    merged.append(name, value);
                }
            }
            "content-type" if !value.is_empty() => {
                if let Ok(value) = HeaderValue::from_bytes(&header.value) {
                    merged.insert(http::header::CONTENT_TYPE, value);
                }
            }
            name => {
                if let Some(added) = name.strip_prefix(HEADER_PREFIX)
                    && !added.is_empty()
                {
                    headers.push((added.to_owned(), header.value.clone()));
                }
            }
        }
    }

    if target.is_none() && flags.accel_redirect {
        target = accel;
    }

    if target.is_none()
        && flags.cgi_local_redirect
        && head.status == 200
        && empty_body
        && location
            .as_deref()
            .is_some_and(|location| location.starts_with('/'))
    {
        target = location;
    }

    let target = target?;

    if !target.starts_with('/') || target.starts_with("//") {
        tracing::warn!(%target, "an internal redirect must name an absolute path on this server; the response is sent as it is");
        return None;
    }

    Some(Redirect {
        target,
        host: host.filter(|name| !name.is_empty()),
        status: u16::try_from(head.status).unwrap_or(500),
        preserve_method: preserve,
        headers,
        merged,
        body,
    })
}

/// The file hint by path a response head carries (section 4.9):
/// X-Muru-Sendfile, or the second header the policy names, with a value.
pub(crate) fn sendfile_from(head: &pb::ResponseHead, policy: &Sendfile) -> Option<String> {
    head.headers.iter().find_map(|header| {
        let name = String::from_utf8_lossy(&header.name).to_ascii_lowercase();

        if name == "x-muru-sendfile" || policy.header.as_deref() == Some(name.as_str()) {
            let value = String::from_utf8_lossy(&header.value).trim().to_owned();
            (!value.is_empty()).then_some(value)
        } else {
            None
        }
    })
}

/// Serve the file a hint names in place of the response it came with
/// (section 4.9), recording the outcome for the access log.
pub(crate) async fn serve_hint(
    path: &str,
    parts: &http::request::Parts,
    context: &Context<'_>,
    head: &pb::ResponseHead,
    failing: &Failing<'_>,
    recording: Option<Recording>,
) -> Response<ResponseBody> {
    let strip: Vec<&str> = std::iter::once("x-muru-sendfile")
        .chain(context.sendfile.header.as_deref())
        .collect();
    let served = crate::static_files::serve_hint(
        path,
        &context.sendfile.roots,
        &parts.method,
        &parts.headers,
        head,
        &strip,
    )
    .await;

    match served {
        Ok(crate::static_files::Outcome::Served {
            response,
            status,
            bytes,
            ..
        }) => {
            if let Some(recording) = recording {
                recording.complete(status, "", bytes);
            }

            response
        }
        Ok(_) => {
            failing.put_recording(recording);
            failing.respond(Problem::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "A file answers GET and HEAD only.",
            ))
        }
        Err(problem) => {
            failing.put_recording(recording);
            failing.respond(problem)
        }
    }
}

/// Whether a frame carries response body: bytes, or a file range whose
/// descriptor the edge has to let go of when it drains the response.
fn carries_body(next: &frame::Frame, reader: &mut Reader) -> bool {
    if next.kind == kind::FILE {
        drop(reader.get_mut().take_fd());
        return true;
    }

    next.kind == kind::BODY
}

/// Read the rest of a response the edge has no use for, so the worker
/// connection goes back to the pool intact (section 4.10).
async fn drain_response(mut reader: Reader, back: &Arc<Return>, idle_timeout: Duration) {
    loop {
        match tokio::time::timeout(idle_timeout, frame::read(&mut reader)).await {
            Ok(Ok(Some(next))) if next.kind == kind::CONTINUE => back.continue_request(),
            Ok(Ok(Some(next))) if carries_body(&next, &mut reader) && !next.is_end() => {}
            Ok(Ok(Some(next))) if next.kind == kind::BODY || next.kind == kind::END => {
                back.give_reader(Some(reader));
                back.abort_request();
                return;
            }
            _ => {
                back.give_reader(None);
                back.abort_request();
                return;
            }
        }
    }
}

/// How this request's failures are answered: with the error pages of
/// section 4.12, in the client's preferred format, and a record in the
/// access log.
pub(crate) struct Failing<'a> {
    request_id: &'a str,
    max_body_size: u64,
    negotiation: Negotiation,
    version: http::Version,
    recording: Mutex<Option<Recording>>,
    /// The custom pages in force (section 4.12).
    errors: &'a errors::Chain,
    /// The host and path the client asked for, for templates.
    host: String,
    path: String,
}

impl Failing<'_> {
    /// The recording, for the response that is not a failure.
    pub(crate) fn take_recording(&self) -> Option<Recording> {
        self.recording.lock().expect("recording poisoned").take()
    }

    /// Hand the recording back, for a failure found after all.
    pub(crate) fn put_recording(&self, recording: Option<Recording>) {
        *self.recording.lock().expect("recording poisoned") = recording;
    }

    /// The client's preferred error format.
    pub(crate) fn negotiation(&self) -> &Negotiation {
        &self.negotiation
    }

    pub(crate) fn respond(&self, problem: Problem) -> Response<ResponseBody> {
        let response = errors::respond(
            &problem,
            &errors::Rendering {
                request_id: self.request_id,
                negotiation: &self.negotiation,
                errors: self.errors,
                host: &self.host,
                path: &self.path,
            },
        );

        if let Some(recording) = self.recording.lock().expect("recording poisoned").take() {
            let bytes = response
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()?.parse().ok())
                .unwrap_or(0);
            recording.complete(problem.status.as_u16(), problem.reason, bytes);
        }

        response
    }

    /// Respond and close an HTTP/1.1 connection, whose state is suspect.
    pub(crate) fn close(&self, problem: Problem) -> Response<ResponseBody> {
        close_after(self.respond(problem), self.version)
    }

    pub(crate) fn too_large(&self) -> Response<ResponseBody> {
        self.close(Problem::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            format!(
                "The request body is larger than max_body_size allows ({} bytes).",
                self.max_body_size
            ),
        ))
    }

    fn worker_crashed(&self, description: &str) -> Response<ResponseBody> {
        self.respond(Problem::new(
            StatusCode::BAD_GATEWAY,
            "worker_crashed",
            description,
        ))
    }

    /// The worker reported a failure instead of a response head.
    fn worker_failed(&self, end: pb::End) -> Response<ResponseBody> {
        let description = if end.error.is_empty() {
            "The worker failed before producing a response.".to_owned()
        } else {
            capitalise(&end.error)
        };

        // Section 7.6: a request the worker's soft timeout interrupted
        // is a 504; any other failure the application raised is a 500.

        let mut problem = match end.reason.as_str() {
            "app_error" => {
                Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "app_error", description)
            }
            "soft_timeout" => {
                Problem::new(StatusCode::GATEWAY_TIMEOUT, "soft_timeout", description)
            }
            _ => Problem::new(StatusCode::BAD_GATEWAY, "worker_crashed", description),
        };

        if !end.detail.is_empty() {
            problem.detail = Some(end.detail);
        }

        self.respond(problem)
    }
}

/// The problem the override makes of an application's response head
/// (section 4.12), if the status is listed, the condition holds and the
/// application did not opt out with X-Muru-Error-Override: off. The
/// response's Retry-After, WWW-Authenticate and Allow are kept.
pub(crate) fn override_from(
    head: &pb::ResponseHead,
    response_end: bool,
    negotiation: &Negotiation,
    errors: &errors::Chain,
) -> Option<Problem> {
    let status = u16::try_from(head.status).ok()?;
    let when = errors.override_for(status)?;
    let header = |wanted: &str| {
        head.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(wanted.as_bytes()))
            .map(|header| header.value.clone())
    };

    if header("x-muru-error-override").is_some_and(|value| value.eq_ignore_ascii_case(b"off")) {
        return None;
    }

    let applies = match when {
        errors::When::All => true,
        errors::When::EmptyBody => {
            response_end || header("content-length").is_some_and(|value| value.as_ref() == b"0")
        }
        errors::When::NotJson => !header("content-type").is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.starts_with(b"application/json") || value.starts_with(b"application/problem+json")
        }),
        errors::When::HtmlOnly => negotiation.format == errors::Format::Html,
    };

    if !applies {
        return None;
    }

    let status = StatusCode::from_u16(status).ok()?;
    let mut problem = Problem::new(
        status,
        errors::OVERRIDE_REASON,
        format!(
            "The application answered {} {}.",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Error")
        ),
    );

    for name in ["retry-after", "www-authenticate", "allow"] {
        if let Some(value) = header(name)
            && let Ok(value) = http::HeaderValue::from_bytes(&value)
        {
            problem
                .headers
                .push((http::HeaderName::from_static(name), value));
        }
    }

    Some(problem)
}

/// Start a sentence with a capital letter and end it with a full stop.
fn capitalise(sentence: &str) -> String {
    let mut out: String = sentence
        .chars()
        .take(1)
        .flat_map(char::to_uppercase)
        .collect();
    out.extend(sentence.chars().skip(1));

    if !out.ends_with('.') {
        out.push('.');
    }

    out
}

/// Why a request body could not be read whole.
pub(crate) enum BodyError {
    /// It is larger than max_body_size.
    TooLarge,
    /// The client's connection failed.
    Read(hyper::Error),
}

/// Read a request body in full, up to the size limit.
/// A deadline as Unix milliseconds, for the request head.
pub(crate) fn unix_deadline(deadline: Instant) -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
        .saturating_add(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64,
        )
}

/// Replace the head's Content-Length with the body the application gets.
fn set_content_length(head: &mut pb::RequestHead, length: u64) {
    head.headers
        .retain(|header| header.name.as_ref() != b"content-length");
    head.headers.push(pb::Header {
        name: Bytes::from_static(b"content-length"),
        value: Bytes::from(length.to_string()),
    });
}

pub(crate) async fn read_body(body: &mut Incoming, max_body_size: u64) -> Result<Bytes, BodyError> {
    let mut data = BytesMut::new();

    while let Some(next) = body.frame().await {
        let next = next.map_err(BodyError::Read)?;

        if let Ok(chunk) = next.into_data() {
            if (data.len() + chunk.len()) as u64 > max_body_size {
                return Err(BodyError::TooLarge);
            }

            data.extend_from_slice(&chunk);
        }
    }

    Ok(data.freeze())
}

/// The response body in flight: the read half until the body ends.
struct BodyState {
    reader: Option<Reader>,
    back: Arc<Return>,
    recording: Option<Recording>,
    bytes: u64,
    idle_timeout: Duration,
    /// A file range a FILE frame handed over, being read (section 5.7).
    file: Option<FileRange>,
}

/// The range of a file the edge is sending for a FILE frame.
struct FileRange {
    file: tokio::fs::File,
    remaining: u64,
    /// The frame ended the body: nothing follows the range.
    last: bool,
}

impl BodyState {
    fn finish(&mut self, intact: bool) {
        let reader = self.reader.take();
        self.back.give_reader(if intact { reader } else { None });
        self.back.abort_request();
        self.record();
    }

    fn record(&mut self) {
        if let Some(recording) = self.recording.take() {
            let status = recording.record.status;
            recording.complete(status, "", self.bytes);
        }
    }
}

impl Drop for BodyState {
    /// hyper drops a body once the declared length is written, which can be
    /// before the END frame has been read. Drain to the end in the
    /// background so the connection can go back to the pool.
    fn drop(&mut self) {
        self.record();

        if let Some(mut reader) = self.reader.take() {
            let back = Arc::clone(&self.back);
            tokio::spawn(async move {
                loop {
                    match frame::read(&mut reader).await {
                        Ok(Some(next))
                            if next.kind == kind::CONTINUE
                                || (carries_body(&next, &mut reader) && !next.is_end()) => {}
                        Ok(Some(next)) if next.kind == kind::BODY || next.kind == kind::END => {
                            back.give_reader(Some(reader));
                            back.abort_request();
                            return;
                        }
                        _ => {
                            back.give_reader(None);
                            back.abort_request();
                            return;
                        }
                    }
                }
            });
        }
    }
}

async fn next_chunk(mut state: BodyState) -> Option<(Result<Frame<Bytes>, BoxError>, BodyState)> {
    loop {
        if let Some(range) = state.file.as_mut() {
            // The range's end is noticed with its last chunk, not on the
            // poll after it: hyper drops a body once the declared length
            // is written, and the connection must be back by then.

            match read_range(range).await {
                Ok(Some(chunk)) => {
                    state.bytes += chunk.len() as u64;

                    if range.remaining == 0 {
                        let last = range.last;
                        state.file = None;

                        if last {
                            state.finish(true);
                        }
                    }

                    return Some((Ok(Frame::data(chunk)), state));
                }
                Ok(None) => {
                    let last = range.last;
                    state.file = None;

                    if last {
                        state.finish(true);
                        return None;
                    }

                    continue;
                }
                Err(error) => {
                    state.file = None;
                    state.finish(false);
                    return Some((Err(error), state));
                }
            }
        }

        let idle_timeout = state.idle_timeout;
        let reader = state.reader.as_mut()?;

        let next = match tokio::time::timeout(idle_timeout, frame::read(reader)).await {
            Ok(next) => next,
            Err(_) => {
                state.finish(false);
                return Some((
                    Err(BoxError::from("the worker stopped sending the response")),
                    state,
                ));
            }
        };

        return match next {
            Ok(Some(next)) if next.kind == kind::CONTINUE => {
                state.back.continue_request();
                continue;
            }
            Ok(Some(next)) if next.kind == kind::BODY => {
                state.bytes += next.payload.len() as u64;

                if next.is_end() {
                    state.finish(true);
                }

                Some((Ok(Frame::data(next.payload)), state))
            }
            Ok(Some(next)) if next.kind == kind::FILE => {
                // Section 5.7: the descriptor arrived with the frame; the
                // range is read from it on the blocking pool.

                let last = next.is_end();
                let hint = pb::FileHint::decode(next.payload).unwrap_or_default();
                let Some(fd) = reader.get_mut().take_fd() else {
                    state.finish(false);
                    return Some((
                        Err(BoxError::from(
                            "the worker sent a file hint without a descriptor",
                        )),
                        state,
                    ));
                };
                let mut file = tokio::fs::File::from_std(std::fs::File::from(fd));

                if hint.offset > 0
                    && let Err(error) = file.seek(std::io::SeekFrom::Start(hint.offset)).await
                {
                    state.finish(false);
                    return Some((Err(BoxError::from(error)), state));
                }

                state.file = Some(FileRange {
                    file,
                    remaining: hint.length,
                    last,
                });
                continue;
            }
            Ok(Some(next)) if next.kind == kind::END => {
                let end = pb::End::decode(next.payload).unwrap_or_default();
                state.finish(true);

                if end.error.is_empty() {
                    None
                } else {
                    Some((Err(BoxError::from(end.error)), state))
                }
            }
            Ok(Some(next)) => {
                state.finish(false);
                let error = format!("unexpected frame type {} in a response body", next.kind);
                Some((Err(BoxError::from(error)), state))
            }
            Ok(None) => {
                state.finish(false);
                Some((
                    Err(BoxError::from("the worker closed the connection")),
                    state,
                ))
            }
            Err(error) => {
                state.finish(false);
                Some((Err(BoxError::from(error)), state))
            }
        };
    }
}

/// The next chunk of a file range, or none once it is all sent. A file
/// shorter than the hint said is an error, since the client was promised
/// the length.
async fn read_range(range: &mut FileRange) -> Result<Option<Bytes>, BoxError> {
    const CHUNK: usize = 64 * 1024;

    if range.remaining == 0 {
        return Ok(None);
    }

    let wanted = usize::try_from(range.remaining.min(CHUNK as u64)).unwrap_or(CHUNK);
    let mut buffer = vec![0u8; wanted];
    let read = range.file.read(&mut buffer).await?;

    if read == 0 {
        return Err(BoxError::from(format!(
            "the file ended {} bytes before the hinted range did",
            range.remaining
        )));
    }

    buffer.truncate(read);
    range.remaining -= read as u64;
    Ok(Some(Bytes::from(buffer)))
}

/// Send the request body as BODY frames, then END. If the response ends
/// first, or the client goes away, send ABORT instead and discard what
/// remains of the body.
async fn pump_body(
    mut body: Incoming,
    mut writer: Writer,
    back: Arc<Return>,
    expect: bool,
    idle_timeout: Duration,
) {
    if expect {
        // hyper sends 100 Continue when the body is first polled, so hold
        // off until the worker says the application is reading. If the
        // response ends first the client is never told to send the body,
        // and there is nothing to discard.

        tokio::select! {
            _ = back.proceed.notified() => {}
            _ = back.abort.notified() => {
                abort(body, writer, back, false).await;
                return;
            }
        }
    }

    loop {
        let next = tokio::select! {
            next = tokio::time::timeout(idle_timeout, body.frame()) => match next {
                Ok(next) => next,
                Err(_) => {
                    // Section 4.6: a client that stops mid-body is treated
                    // as gone; the response path answers with 408 if the
                    // worker has not responded yet.

                    back.client_stalled.store(true, Ordering::Relaxed);
                    abort(body, writer, back, false).await;
                    return;
                }
            },
            _ = back.abort.notified() => {
                abort(body, writer, back, true).await;
                return;
            }
        };

        match next {
            Some(Ok(next)) => {
                if let Ok(data) = next.into_data()
                    && !data.is_empty()
                    && frame::write(&mut writer, kind::BODY, 0, data)
                        .await
                        .is_err()
                {
                    back.give_writer(None);
                    return;
                }
            }
            Some(Err(_)) => {
                // The client went away mid-body. The worker learns by ABORT
                // and the connection stays in the pool.

                abort(body, writer, back, true).await;
                return;
            }
            None => break,
        }
    }

    let end = pb::End::default().encode_to_vec();
    if frame::write(&mut writer, kind::END, 0, end.into())
        .await
        .is_err()
    {
        back.give_writer(None);
        return;
    }
    back.give_writer(Some(writer));
}

/// End the request direction with ABORT and return the writer. When the
/// client may be sending the body, discard what it still sends within the
/// limits, so that its connection can be kept alive when the body was
/// small. A client still waiting for 100 Continue has sent nothing; its
/// body is left unread and hyper closes the connection after the
/// response, since the body may yet follow.
async fn abort(body: Incoming, mut writer: Writer, back: Arc<Return>, discard_rest: bool) {
    let payload = pb::Abort::default().encode_to_vec();

    match frame::write(&mut writer, kind::ABORT, 0, payload.into()).await {
        Ok(()) => back.give_writer(Some(writer)),
        Err(_) => back.give_writer(None),
    }

    // hyper closes the client connection after the response if the body is
    // left unread, so read a bounded amount first. Dropping the body past
    // the limit lets hyper close it.

    if discard_rest {
        let _ = tokio::time::timeout(DISCARD_DELAY, discard(body)).await;
    }
}

async fn discard(mut body: Incoming) {
    let mut seen = 0;

    while let Some(Ok(next)) = body.frame().await {
        if let Ok(data) = next.into_data() {
            seen += data.len();

            if seen > DISCARD_LIMIT {
                return;
            }
        }
    }
}
