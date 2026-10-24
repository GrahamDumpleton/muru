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
use http::{Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::body::{Body, Frame, Incoming};
use muru_proto::frame::{self, flag, kind};
use muru_proto::worker::v1 as pb;
use prost::Message;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::access::{AccessLog, Record};
use crate::errors::{self, Negotiation, Problem};
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

pub(crate) type Reader = BufReader<OwnedReadHalf>;
pub(crate) type Writer = OwnedWriteHalf;

/// One idle connection to a worker.
struct Conn {
    reader: Reader,
    writer: Writer,
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
}

/// A connection taken from the pool for one request.
struct Lease {
    conn: Conn,
    permit: OwnedSemaphorePermit,
    worker: usize,
    /// Whether the worker accepts websocket scopes.
    websockets: bool,
}

impl LinkPool {
    /// Connect once to every worker socket and size its pool from what its
    /// HELLO declares (section 5.1): the edge acts on the capabilities, not
    /// on the kind of worker. The first connection stays in the pool.
    pub async fn connect(sockets: &[PathBuf]) -> anyhow::Result<Self> {
        let mut workers = Vec::with_capacity(sockets.len());
        for path in sockets {
            let (conn, hello) = connect(path)
                .await
                .with_context(|| format!("connecting to the worker at {}", path.display()))?;

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
        }
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
    async fn acquire(&self) -> io::Result<Lease> {
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
        let permit = Arc::clone(&worker.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("pool closed"))?;
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
    let mut reader = BufReader::with_capacity(32 * 1024, read_half);
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
    abort: Notify,
    /// Signalled when the worker has sent CONTINUE: the application is
    /// reading the body, so a waiting client may be told to send it.
    proceed: Notify,
    proceeded: AtomicBool,
    /// Set when the client stopped sending its body for too long.
    client_stalled: AtomicBool,
    /// Set once the response direction has ended, intact or not.
    ended: AtomicBool,
}

struct ReturnState {
    reader: Option<Reader>,
    writer: Option<Writer>,
    broken: bool,
    reported: u8,
    permit: Option<OwnedSemaphorePermit>,
}

impl Return {
    fn new(pool: Arc<LinkPool>, worker: usize, permit: OwnedSemaphorePermit) -> Arc<Self> {
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
        })
    }

    /// Tell the body pump, if it is still running, to send ABORT and stop.
    fn abort_request(&self) {
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
    /// The client's address.
    pub peer: SocketAddr,
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
}

/// The access log record of a request in progress.
pub(crate) struct Recording {
    log: Arc<AccessLog>,
    record: Record,
    started: Instant,
}

impl Recording {
    fn new(context: &Context<'_>, parts: &http::request::Parts) -> Self {
        Self {
            log: Arc::clone(&context.access_log),
            record: Record {
                started: SystemTime::now(),
                request_id: context.request_id.to_owned(),
                client: context.peer.ip(),
                method: parts.method.as_str().to_owned(),
                target: parts
                    .uri
                    .path_and_query()
                    .map_or_else(|| parts.uri.path().to_owned(), |target| target.to_string()),
                version: format!("{:?}", parts.version),
                status: 0,
                reason: String::new(),
                bytes: 0,
                worker: None,
                queue: Duration::ZERO,
                application: Duration::ZERO,
                total: Duration::ZERO,
            },
            started: Instant::now(),
        }
    }

    pub(crate) fn complete(mut self, status: u16, reason: &str, bytes: u64) {
        self.record.status = status;
        self.record.reason = reason.to_owned();
        self.record.bytes = bytes;
        self.record.total = self.started.elapsed();
        self.log.record(self.record);
    }
}

/// Answer a plain HTTP request with a redirect to the HTTPS listener
/// (--https-only, section 13.1). 307 keeps the method and is never cached,
/// so a later run without TLS on the same port is unaffected.
/// Answer a request the routing tables refuse (421 or 404), recording it
/// in the access log like any other edge-generated response.
pub(crate) fn reject(
    request: http::Request<Incoming>,
    context: &Context<'_>,
    problem: Problem,
) -> Response<ResponseBody> {
    let (parts, _body) = request.into_parts();
    let recording = Recording::new(context, &parts);
    let failing = Failing {
        request_id: context.request_id,
        max_body_size: context.limits.max_body_size,
        negotiation: Negotiation::from_headers(&parts.headers, context.development),
        version: parts.version,
        recording: Mutex::new(Some(recording)),
    };

    failing.respond(problem)
}

pub(crate) fn redirect_to_https(
    request: http::Request<Incoming>,
    context: &Context<'_>,
    to: SocketAddr,
) -> Response<ResponseBody> {
    let (parts, _body) = request.into_parts();
    let recording = Recording::new(context, &parts);
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

/// A Host header value without its port: `shop.localhost:8080` becomes
/// `shop.localhost` and `[::1]:8080` becomes `[::1]`.
pub async fn handle(
    request: http::Request<Incoming>,
    pool: Arc<LinkPool>,
    context: &Context<'_>,
) -> Response<ResponseBody> {
    let (mut parts, mut body) = request.into_parts();
    let upgrade = crate::websocket::detect(&mut parts);
    let mut head = request_head(&parts, context);
    let limits = &context.limits;
    let arrived = Instant::now();
    let deadline = arrived + limits.request_deadline;
    head.deadline_unix_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
        .saturating_add(limits.request_deadline.as_millis() as u64);
    let mut recording = Recording::new(context, &parts);
    let failing = Failing {
        request_id: context.request_id,
        max_body_size: limits.max_body_size,
        negotiation: Negotiation::from_headers(&parts.headers, context.development),
        version: parts.version,
        recording: Mutex::new(None),
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
    // send 100 Continue to a client that is waiting for it.

    let mut buffered: Option<Bytes> = None;
    if !body.is_end_stream() && head.content_length.is_none() {
        match read_body(&mut body, limits.max_body_size).await {
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
    let lease = match tokio::time::timeout(limits.queue_timeout, pool.acquire()).await {
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
        || buffered
            .as_ref()
            .map_or_else(|| body.is_end_stream(), Bytes::is_empty);
    let flags = if end { flag::END } else { 0 };
    *failing.recording.lock().expect("recording poisoned") = Some(recording);
    if let Err(error) = frame::write(
        &mut writer,
        kind::REQUEST,
        flags,
        head.encode_to_vec().into(),
    )
    .await
    {
        tracing::warn!(%error, "writing the request head failed");
        drop(permit);
        return failing.worker_crashed("The worker is not available.");
    }

    let back = Return::new(pool, worker, permit);
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
        None => {
            tokio::spawn(pump_body(
                body,
                writer,
                Arc::clone(&back),
                expect_continue,
                limits.body_idle_timeout,
            ));
        }
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

/// How this request's failures are answered: with the error pages of
/// section 4.12, in the client's preferred format, and a record in the
/// access log.
struct Failing<'a> {
    request_id: &'a str,
    max_body_size: u64,
    negotiation: Negotiation,
    version: http::Version,
    recording: Mutex<Option<Recording>>,
}

impl Failing<'_> {
    fn respond(&self, problem: Problem) -> Response<ResponseBody> {
        let response = errors::respond(&problem, self.request_id, &self.negotiation);
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
    fn close(&self, problem: Problem) -> Response<ResponseBody> {
        close_after(self.respond(problem), self.version)
    }

    fn too_large(&self) -> Response<ResponseBody> {
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
        let mut problem = if end.reason == "app_error" {
            Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "app_error", description)
        } else {
            Problem::new(StatusCode::BAD_GATEWAY, "worker_crashed", description)
        };
        if !end.detail.is_empty() {
            problem.detail = Some(end.detail);
        }
        self.respond(problem)
    }
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

enum BodyError {
    TooLarge,
    Read(hyper::Error),
}

/// Read a request body in full, up to the size limit.
async fn read_body(body: &mut Incoming, max_body_size: u64) -> Result<Bytes, BodyError> {
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
                                || (next.kind == kind::BODY && !next.is_end()) => {}
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
