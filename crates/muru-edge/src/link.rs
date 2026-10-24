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
use std::time::Duration;

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

use crate::{BoxError, ResponseBody, close_after, error_response, request_head};

/// After the response has ended, this much of an unread request body is
/// read and discarded so the client connection can be kept alive; beyond
/// it, or after the delay, the connection is closed instead (section 4.6).
const DISCARD_LIMIT: usize = 1024 * 1024;
const DISCARD_DELAY: Duration = Duration::from_secs(2);

/// The largest request body accepted (max_body_size in section 4.6, with
/// the same default as mod_wsgi-express). A declared length beyond it is
/// refused before anything is read; an undeclared one as soon as it is
/// exceeded.
pub const MAX_BODY_SIZE: u64 = 10 * 1024 * 1024;

type Reader = BufReader<OwnedReadHalf>;
type Writer = OwnedWriteHalf;

/// One idle connection to a worker.
struct Conn {
    reader: Reader,
    writer: Writer,
}

struct WorkerSlots {
    path: PathBuf,
    semaphore: Arc<Semaphore>,
    idle: Mutex<Vec<Conn>>,
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
}

impl LinkPool {
    /// Create one pool per worker socket, each bounded to `slots` requests.
    pub fn new(sockets: &[PathBuf], slots: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !sockets.is_empty(),
            "at least one worker socket is required"
        );
        Ok(Self {
            workers: sockets
                .iter()
                .map(|path| WorkerSlots {
                    path: path.clone(),
                    semaphore: Arc::new(Semaphore::new(slots.max(1))),
                    idle: Mutex::new(Vec::new()),
                })
                .collect(),
            next: AtomicUsize::new(0),
        })
    }

    /// Take a connection from the worker with the most free slots, waiting
    /// for one if every worker is busy.
    async fn acquire(&self) -> io::Result<Lease> {
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
            None => connect(&worker.path).await?,
        };
        Ok(Lease {
            conn,
            permit,
            worker: best,
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

async fn connect(path: &PathBuf) -> io::Result<Conn> {
    let stream = UnixStream::connect(path).await?;
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::with_capacity(32 * 1024, read_half);
    let mut writer = write_half;
    let hello = pb::Hello {
        protocol_version: pb::PROTOCOL_VERSION.to_owned(),
        ..Default::default()
    };
    frame::write(&mut writer, kind::HELLO, 0, hello.encode_to_vec().into()).await?;
    match frame::read(&mut reader).await? {
        Some(reply) if reply.kind == kind::HELLO => {
            pb::Hello::decode(reply.payload)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the worker did not answer HELLO",
            ));
        }
    }
    Ok(Conn { reader, writer })
}

/// Collects the two halves of a leased connection as each direction
/// finishes, and returns the connection to the pool when both are back
/// intact. A half reported as broken closes the connection instead.
struct Return {
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

    fn give_reader(&self, reader: Option<Reader>) {
        let mut state = self.state.lock().expect("return state poisoned");
        state.reader = reader;
        self.report(&mut state);
    }

    fn give_writer(&self, writer: Option<Writer>) {
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

/// Forward one request to a worker over the framed link.
pub async fn handle(
    request: http::Request<Incoming>,
    pool: Arc<LinkPool>,
    request_id: u64,
    peer: SocketAddr,
    local: SocketAddr,
) -> Response<ResponseBody> {
    let (parts, mut body) = request.into_parts();
    let mut head = request_head(&parts, request_id, peer, local);

    if head
        .content_length
        .is_some_and(|length| length > MAX_BODY_SIZE)
    {
        return too_large();
    }

    // Section 4.6: a body without a declared length is read in full first,
    // so that the application sees CONTENT_LENGTH and no worker slot is
    // held while it arrives. Polling the body is also what makes hyper
    // send 100 Continue to a client that is waiting for it.
    let mut buffered: Option<Bytes> = None;
    if !body.is_end_stream() && head.content_length.is_none() {
        match read_body(&mut body).await {
            Ok(data) => {
                head.content_length = Some(data.len() as u64);
                head.body_buffered = true;
                buffered = Some(data);
            }
            Err(BodyError::TooLarge) => return too_large(),
            Err(BodyError::Read(error)) => {
                tracing::debug!(%error, "reading the request body failed");
                return close_after(error_response(
                    StatusCode::BAD_REQUEST,
                    "the request body could not be read",
                ));
            }
        }
    }
    let expect_continue = head.expect_continue && !head.body_buffered;

    let lease = match pool.acquire().await {
        Ok(lease) => lease,
        Err(error) => {
            tracing::warn!(%error, "no worker connection");
            return error_response(StatusCode::SERVICE_UNAVAILABLE, "no worker is available");
        }
    };
    let Lease {
        conn,
        permit,
        worker,
    } = lease;
    let Conn {
        mut reader,
        mut writer,
    } = conn;

    let end = buffered
        .as_ref()
        .map_or_else(|| body.is_end_stream(), Bytes::is_empty);
    let flags = if end { flag::END } else { 0 };
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
        return error_response(StatusCode::BAD_GATEWAY, "the worker is not available");
    }

    let back = Return::new(pool, worker, permit);
    match buffered {
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
            tokio::spawn(pump_body(body, writer, Arc::clone(&back), expect_continue));
        }
    }

    let response = loop {
        match frame::read(&mut reader).await {
            Ok(Some(next)) if next.kind == kind::CONTINUE => back.continue_request(),
            Ok(Some(next)) if next.kind == kind::RESPONSE => break next,
            Ok(_) => {
                back.give_reader(None);
                return error_response(StatusCode::BAD_GATEWAY, "the worker sent no response head");
            }
            Err(error) => {
                tracing::warn!(%error, "reading the response head failed");
                back.give_reader(None);
                return error_response(StatusCode::BAD_GATEWAY, "the worker failed");
            }
        }
    };
    let response_end = response.is_end();
    let Ok(head) = pb::ResponseHead::decode(response.payload) else {
        back.give_reader(None);
        return error_response(StatusCode::BAD_GATEWAY, "invalid response head");
    };

    let mut builder = Response::builder().status(crate::status_code(head.status));
    if let Some(headers) = builder.headers_mut() {
        crate::copy_headers(headers, head.headers);
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
        let empty = Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync();
        builder.body(empty)
    } else {
        let stream = futures::stream::unfold(
            BodyState {
                reader: Some(reader),
                back,
            },
            next_chunk,
        );
        builder.body(UnsyncBoxBody::new(StreamBody::new(stream)))
    };
    let response = response
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "invalid response head"));
    if close {
        close_after(response)
    } else {
        response
    }
}

fn too_large() -> Response<ResponseBody> {
    let message = format!(
        "the request body is larger than max_body_size allows ({} bytes)",
        MAX_BODY_SIZE
    );
    close_after(error_response(StatusCode::PAYLOAD_TOO_LARGE, &message))
}

enum BodyError {
    TooLarge,
    Read(hyper::Error),
}

/// Read a request body in full, up to the size limit.
async fn read_body(body: &mut Incoming) -> Result<Bytes, BodyError> {
    let mut data = BytesMut::new();
    while let Some(next) = body.frame().await {
        let next = next.map_err(BodyError::Read)?;
        if let Ok(chunk) = next.into_data() {
            if (data.len() + chunk.len()) as u64 > MAX_BODY_SIZE {
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
}

impl BodyState {
    fn finish(&mut self, intact: bool) {
        let reader = self.reader.take();
        self.back.give_reader(if intact { reader } else { None });
        self.back.abort_request();
    }
}

impl Drop for BodyState {
    /// hyper drops a body once the declared length is written, which can be
    /// before the END frame has been read. Drain to the end in the
    /// background so the connection can go back to the pool.
    fn drop(&mut self) {
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
        let reader = state.reader.as_mut()?;
        return match frame::read(reader).await {
            Ok(Some(next)) if next.kind == kind::CONTINUE => {
                state.back.continue_request();
                continue;
            }
            Ok(Some(next)) if next.kind == kind::BODY => {
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
async fn pump_body(mut body: Incoming, mut writer: Writer, back: Arc<Return>, expect: bool) {
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
            next = body.frame() => next,
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
