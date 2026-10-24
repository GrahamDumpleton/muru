//! The ASGI bridge: HTTP requests run as tasks on the worker's event loop
//! (design specification section 6.6).
//!
//! The loop is Python's own. `muru._asgi` starts it on a thread after the
//! fork and hands `serve` the loop, a runner that schedules one task per
//! request, and the resolver that completes futures. The link task delivers
//! request events through a [`Channel`], the object behind the
//! application's `receive` and `send`: `receive()` returns a future that
//! the link completes from Rust with the next message, and `send()`
//! validates a message and queues it for the coalescing writer, returning a
//! future only when the writer is behind and the application must wait.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use muru_proto::worker::v1 as pb;
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::pool::Shared;
use crate::wsgi::{self, Outgoing};

/// What `serve` needs from `muru._asgi` to run an ASGI application.
pub struct Asgi {
    /// The worker's asyncio event loop, running on its own thread.
    pub event_loop: Py<PyAny>,
    /// Called on the loop with (scope, channel) to start a request's task.
    pub runner: Py<PyAny>,
    /// Completes a future with a value unless it is already done.
    pub resolver: Py<PyAny>,
    /// How many requests the loop holds at once: the async slots in HELLO.
    pub concurrency: usize,
}

/// A message on its way to the application's `receive()`.
enum Message {
    /// A chunk of the request body, the last when `more` is false.
    Body { data: Bytes, more: bool },
    /// The client went away, or the response is complete.
    Disconnect,
}

/// Messages waiting for the application, and the application waiting for
/// a message.
struct Inbox {
    queue: VecDeque<Message>,
    /// A future from `receive()` waiting for the next message.
    waiter: Option<Py<PyAny>>,
    /// No more body will arrive: every later `receive()` gets a disconnect.
    closed: bool,
}

/// What the application has sent so far.
#[derive(Default)]
struct SendState {
    head_sent: bool,
    body_ended: bool,
}

/// Body chunks held for the application before the link stops reading.
const INBOX_CAPACITY: usize = 8;

/// The link side of one ASGI request: `receive` and `send` for the application.
#[pyclass(frozen, module = "muru._native")]
pub struct Channel {
    inbox: Mutex<Inbox>,
    state: Mutex<SendState>,
    tx: mpsc::Sender<Outgoing>,
    runtime: tokio::runtime::Handle,
    event_loop: Py<PyAny>,
    resolver: Py<PyAny>,
    /// One permit per queued body chunk; `receive()` gives it back.
    permits: Arc<tokio::sync::Semaphore>,
    /// Signalled once the response is over, so the link stops holding chunks.
    complete: tokio::sync::Notify,
    completed: AtomicBool,
    /// The client is waiting for 100 Continue until the application reads.
    announce: AtomicBool,
}

impl Channel {
    fn new(py: Python<'_>, asgi: &Asgi, tx: mpsc::Sender<Outgoing>, expect_continue: bool) -> Self {
        Self {
            inbox: Mutex::new(Inbox {
                queue: VecDeque::new(),
                waiter: None,
                closed: false,
            }),
            state: Mutex::new(SendState::default()),
            tx,
            runtime: tokio::runtime::Handle::current(),
            event_loop: asgi.event_loop.clone_ref(py),
            resolver: asgi.resolver.clone_ref(py),
            permits: Arc::new(tokio::sync::Semaphore::new(INBOX_CAPACITY)),
            complete: tokio::sync::Notify::new(),
            completed: AtomicBool::new(false),
            announce: AtomicBool::new(expect_continue),
        }
    }

    /// Hand a body chunk to the application, waiting for room in the inbox.
    ///
    /// Returns false once the response is complete, when nothing more is
    /// delivered and the caller only drains the link.
    pub async fn deliver(&self, data: Bytes, more: bool) -> bool {
        // Wait for room, unless the response ends first.
        let permit = tokio::select! {
            permit = self.permits.acquire() => permit,
            _ = self.complete.notified() => return false,
        };
        if self.completed.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(permit) = permit else {
            return false;
        };

        // The permit is returned by receive() when the chunk leaves the queue.
        permit.forget();
        self.push(Message::Body { data, more });
        true
    }

    /// The client has gone, or the response is complete: the next
    /// `receive()` and every one after it gets `http.disconnect`.
    pub fn disconnect(&self) {
        let mut inbox = self.inbox.lock().expect("inbox poisoned");
        if inbox.closed {
            return;
        }
        inbox.closed = true;

        if let Some(waiter) = inbox.waiter.take() {
            drop(inbox);
            self.wake(waiter, &Message::Disconnect);
        }
    }

    /// The response is over: release the link from holding body chunks.
    pub fn complete(&self) {
        self.completed.store(true, Ordering::Relaxed);
        self.complete.notify_waiters();
        self.complete.notify_one();
    }

    /// Queue a message, or hand it straight to a waiting `receive()`.
    fn push(&self, message: Message) {
        let mut inbox = self.inbox.lock().expect("inbox poisoned");
        match inbox.waiter.take() {
            Some(waiter) => {
                drop(inbox);

                // Delivered straight to the waiting receive(), so the chunk
                // never sits in the queue and its permit comes back now.
                self.permits.add_permits(1);
                self.wake(waiter, &message);
            }
            None => inbox.queue.push_back(message),
        }
    }

    /// Complete a waiting future on the loop's thread.
    fn wake(&self, waiter: Py<PyAny>, message: &Message) {
        Python::attach(|py| {
            let delivered = message_dict(py, message).and_then(|value| {
                self.event_loop.bind(py).call_method1(
                    "call_soon_threadsafe",
                    (self.resolver.bind(py), waiter.bind(py), value),
                )
            });

            // The loop refusing the call means it is closed: the worker is
            // stopping and the request with it.
            if let Err(error) = delivered {
                tracing::debug!(%error, "waking an ASGI receive() failed");
            }
        });
    }

    /// Queue output for the writer.
    ///
    /// Returns a future to await when the writer is behind, or None when
    /// the items went straight through, or when the request is already over
    /// and the output is dropped.
    fn enqueue(&self, py: Python<'_>, items: Vec<Outgoing>) -> PyResult<Option<Py<PyAny>>> {
        let mut items = items.into_iter();
        while let Some(item) = items.next() {
            let item = match self.tx.try_send(item) {
                Ok(()) => continue,
                Err(TrySendError::Closed(_)) => return Ok(None),
                Err(TrySendError::Full(item)) => item,
            };

            // The writer is behind. The rest of the items go out from a
            // task once there is room, and the application waits on a
            // future that the task completes when they have.
            let future = self.event_loop.bind(py).call_method0("create_future")?;
            let waiter = future.clone().unbind();
            let tx = self.tx.clone();
            let event_loop = self.event_loop.clone_ref(py);
            let resolver = self.resolver.clone_ref(py);
            let rest: Vec<Outgoing> = std::iter::once(item).chain(items).collect();
            self.runtime.spawn(async move {
                for item in rest {
                    if tx.send(item).await.is_err() {
                        break;
                    }
                }

                Python::attach(|py| {
                    let woken = event_loop.bind(py).call_method1(
                        "call_soon_threadsafe",
                        (resolver.bind(py), waiter.bind(py), py.None()),
                    );
                    if let Err(error) = woken {
                        tracing::debug!(%error, "waking an ASGI send() failed");
                    }
                });
            });
            return Ok(Some(future.unbind()));
        }

        Ok(None)
    }

    /// End the response with an error: the edge reports it if no output had
    /// reached it yet, and truncates the response otherwise.
    fn abort(&self, py: Python<'_>, error: &str, detail: String) {
        let items = vec![
            Outgoing::Abort {
                error: error.to_owned(),
                reason: "app_error".to_owned(),
                detail,
            },
            Outgoing::Finished,
        ];
        let _ = self.enqueue(py, items);
    }
}

#[pymethods]
impl Channel {
    /// Return a future for the next event: `http.request` with a body
    /// chunk, then `http.disconnect` once the client has gone or the
    /// response is complete.
    fn receive(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        // The application is reading: the client may send the body it is
        // holding back (section 5.5).
        if self.announce.swap(false, Ordering::Relaxed) {
            let _ = self.tx.try_send(Outgoing::Continue);
        }

        // Complete the future now when a message is already there, else
        // leave it for the link to complete.
        let future = self.event_loop.bind(py).call_method0("create_future")?;
        let mut inbox = self.inbox.lock().expect("inbox poisoned");
        if let Some(message) = inbox.queue.pop_front() {
            drop(inbox);
            self.permits.add_permits(1);
            future.call_method1("set_result", (message_dict(py, &message)?,))?;
        } else if inbox.closed {
            drop(inbox);
            future.call_method1("set_result", (message_dict(py, &Message::Disconnect)?,))?;
        } else {
            inbox.waiter = Some(future.clone().unbind());
        }

        Ok(future.unbind())
    }

    /// Take one response message. Returns None when it was queued, or a
    /// future to await when the writer is behind.
    fn send(&self, py: Python<'_>, message: &Bound<'_, PyDict>) -> PyResult<Option<Py<PyAny>>> {
        let kind = message
            .get_item("type")?
            .ok_or_else(|| PyTypeError::new_err("ASGI message has no type"))?
            .extract::<String>()
            .map_err(|_| PyTypeError::new_err("ASGI message type must be a str"))?;

        let mut state = self.state.lock().expect("send state poisoned");
        match kind.as_str() {
            "http.response.start" => {
                if state.head_sent {
                    return Err(PyRuntimeError::new_err(
                        "http.response.start sent after the response started",
                    ));
                }

                let status = message
                    .get_item("status")?
                    .ok_or_else(|| PyTypeError::new_err("http.response.start has no status"))?
                    .extract::<i64>()
                    .map_err(|_| {
                        PyTypeError::new_err("http.response.start status must be an int")
                    })?;
                if !(100..600).contains(&status) {
                    return Err(PyValueError::new_err(format!(
                        "invalid HTTP status {status}"
                    )));
                }
                let headers = match message.get_item("headers")? {
                    Some(headers) => validate_headers(&headers)?,
                    None => Vec::new(),
                };

                state.head_sent = true;
                drop(state);
                let flush = wsgi::flush_policy(&headers);
                self.enqueue(
                    py,
                    vec![Outgoing::Head {
                        status: status as u32,
                        reason: String::new(),
                        headers,
                        flush,
                    }],
                )
            }
            "http.response.body" => {
                if !state.head_sent {
                    return Err(PyRuntimeError::new_err(
                        "http.response.body sent before http.response.start",
                    ));
                }
                if state.body_ended {
                    return Err(PyRuntimeError::new_err(
                        "http.response.body sent after the response was completed",
                    ));
                }

                let more = match message.get_item("more_body")? {
                    Some(more) => more.is_truthy()?,
                    None => false,
                };
                let mut items = Vec::with_capacity(2);
                if let Some(body) = message.get_item("body")? {
                    let Ok(body) = body.cast::<PyBytes>() else {
                        return Err(PyTypeError::new_err(
                            "http.response.body body must be bytes",
                        ));
                    };
                    if !body.as_bytes().is_empty() {
                        items.push(Outgoing::Body(wsgi::bytes_from_py(body)));
                    }
                }

                // The last chunk ends the response, whatever the application
                // does afterwards.
                if !more {
                    state.body_ended = true;
                    items.push(Outgoing::Finished);
                }
                drop(state);
                self.enqueue(py, items)
            }
            "http.response.trailers" | "http.response.pathsend" | "http.response.zerocopysend" => {
                Err(PyRuntimeError::new_err(format!(
                    "{kind} is not advertised in scope['extensions'] and cannot be sent"
                )))
            }
            other => Err(PyRuntimeError::new_err(format!(
                "unexpected ASGI message type {other:?} on an http connection"
            ))),
        }
    }

    /// Record that the application returned. A response that never
    /// started, or never finished, is an error the edge reports.
    fn finish(&self, py: Python<'_>) {
        let state = self.state.lock().expect("send state poisoned");
        let (head_sent, body_ended) = (state.head_sent, state.body_ended);
        drop(state);

        if body_ended {
            return;
        }
        let error = if head_sent {
            "the application returned before completing the response"
        } else {
            "the application returned without sending a response"
        };
        self.abort(py, error, String::new());
    }

    /// Fail the response because the application raised. `detail` is the
    /// formatted traceback, shown by the edge in development mode when no
    /// output had reached it.
    fn fail(&self, py: Python<'_>, detail: String) {
        let body_ended = self.state.lock().expect("send state poisoned").body_ended;
        if body_ended {
            return;
        }

        self.abort(py, "the application raised an exception", detail);
    }
}

/// Complete `future` with `value` unless it is already done (cancelled,
/// for instance). Scheduled on the loop with call_soon_threadsafe.
#[pyfunction]
pub fn resolve_future(future: &Bound<'_, PyAny>, value: &Bound<'_, PyAny>) -> PyResult<()> {
    if !future.call_method0("done")?.is_truthy()? {
        future.call_method1("set_result", (value,))?;
    }

    Ok(())
}

/// Build the dictionary the application receives for a message.
fn message_dict<'py>(py: Python<'py>, message: &Message) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    match message {
        Message::Body { data, more } => {
            dict.set_item("type", "http.request")?;
            dict.set_item("body", PyBytes::new(py, data))?;
            dict.set_item("more_body", *more)?;
        }
        Message::Disconnect => {
            dict.set_item("type", "http.disconnect")?;
        }
    }

    Ok(dict)
}

/// Check the headers of http.response.start: an iterable of (bytes, bytes)
/// pairs with token names and values free of CR and LF.
fn validate_headers(headers: &Bound<'_, PyAny>) -> PyResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut out = Vec::new();
    for item in headers.try_iter()? {
        // Each header is a two-item iterable of bytes.
        let item = item?;
        let mut parts = item.try_iter()?;
        let (Some(name), Some(value), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(PyValueError::new_err(
                "each header must be a (name, value) pair",
            ));
        };
        let (name, value) = (name?, value?);
        let (Ok(name), Ok(value)) = (name.cast::<PyBytes>(), value.cast::<PyBytes>()) else {
            return Err(PyTypeError::new_err(
                "header names and values must be bytes",
            ));
        };
        let name = name.as_bytes().to_vec();
        let value = value.as_bytes().to_vec();

        // The same rules as start_response applies to WSGI headers.
        if name.is_empty() {
            return Err(PyValueError::new_err("header name is empty"));
        }
        if name.iter().any(|&byte| byte <= b' ' || byte == 0x7f) {
            return Err(PyValueError::new_err(format!(
                "header name contains a space or control character: {:?}",
                String::from_utf8_lossy(&name)
            )));
        }
        if value.contains(&b'\r') || value.contains(&b'\n') {
            return Err(PyValueError::new_err(format!(
                "header value contains a carriage return or line feed: {:?}",
                String::from_utf8_lossy(&value)
            )));
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            let text = std::str::from_utf8(&value).unwrap_or("");
            if text.trim().parse::<u64>().is_err() {
                return Err(PyValueError::new_err(format!(
                    "invalid Content-Length header value {text:?}"
                )));
            }
        }

        out.push((name, value));
    }

    Ok(out)
}

/// Build the http scope (Appendix D.2) from the request head.
fn build_scope<'py>(py: Python<'py>, head: &pb::RequestHead) -> PyResult<Bound<'py, PyDict>> {
    let scope = PyDict::new(py);
    scope.set_item("type", "http")?;
    let asgi = PyDict::new(py);
    asgi.set_item("version", "3.0")?;
    asgi.set_item("spec_version", "2.4")?;
    scope.set_item("asgi", asgi)?;

    // The request line, as ASGI wants it: the path decoded, the raw path
    // and query as bytes, the mount prefix as root_path.
    let version = head
        .http_version
        .strip_prefix("HTTP/")
        .filter(|version| !version.is_empty())
        .unwrap_or("1.1");
    scope.set_item("http_version", version)?;
    scope.set_item("method", &head.method)?;
    let scheme = if head.scheme.is_empty() {
        "http"
    } else {
        head.scheme.as_str()
    };
    scope.set_item("scheme", scheme)?;
    let path = wsgi::percent_decode(&head.raw_path);
    scope.set_item("path", String::from_utf8_lossy(&path).as_ref())?;
    scope.set_item("raw_path", PyBytes::new(py, &head.raw_path))?;
    scope.set_item("query_string", PyBytes::new(py, &head.query))?;
    scope.set_item("root_path", &head.script_name)?;

    // Headers as (name, value) byte pairs, names already lower case. A body
    // the edge buffered has a length even when the client sent none
    // (chunked_body = buffer in section 4.6).
    let headers = PyList::empty(py);
    let mut has_length = false;
    for header in &head.headers {
        has_length |= header.name.eq_ignore_ascii_case(b"content-length");
        headers.append(PyTuple::new(
            py,
            [
                PyBytes::new(py, &header.name),
                PyBytes::new(py, &header.value),
            ],
        )?)?;
    }
    if let Some(length) = head.content_length
        && !has_length
    {
        headers.append(PyTuple::new(
            py,
            [
                PyBytes::new(py, b"content-length"),
                PyBytes::new(py, length.to_string().as_bytes()),
            ],
        )?)?;
    }
    scope.set_item("headers", headers)?;

    // The peers, and the state the runner fills from the lifespan.
    scope.set_item(
        "client",
        head.client
            .as_ref()
            .map(|peer| (peer.address.as_str(), peer.port)),
    )?;
    scope.set_item(
        "server",
        head.server
            .as_ref()
            .map(|peer| (peer.address.as_str(), peer.port)),
    )?;
    scope.set_item("state", PyDict::new(py))?;

    // Muru's own values live under extensions, with the names of the
    // matching environ keys.
    let muru = PyDict::new(py);
    muru.set_item("request_id", &head.request_id)?;
    if head.max_body_size != 0 {
        muru.set_item("max_body_size", head.max_body_size)?;
    }
    if let Some(tls) = &head.tls {
        let details = PyDict::new(py);
        details.set_item("version", &tls.version)?;
        details.set_item("cipher", &tls.cipher)?;
        details.set_item("server_name", &tls.server_name)?;
        details.set_item("alpn", &tls.alpn)?;
        muru.set_item("tls", details)?;
    }
    let extensions = PyDict::new(py);
    extensions.set_item("muru", muru)?;
    scope.set_item("extensions", extensions)?;

    Ok(scope)
}

/// Start a request on the event loop: build its scope and channel and
/// schedule the runner. Fails when the loop is gone, which means the
/// worker is stopping.
pub fn start(
    shared: &Shared,
    asgi: &Asgi,
    head: &pb::RequestHead,
    tx: mpsc::Sender<Outgoing>,
) -> Result<Py<Channel>, String> {
    let _ = shared;
    Python::attach(|py| {
        let channel = Bound::new(py, Channel::new(py, asgi, tx, head.expect_continue))
            .map_err(|error| error.to_string())?;
        let scope = build_scope(py, head).map_err(|error| error.to_string())?;

        asgi.event_loop
            .bind(py)
            .call_method1(
                "call_soon_threadsafe",
                (asgi.runner.bind(py), scope, &channel),
            )
            .map_err(|error| error.to_string())?;
        Ok(channel.unbind())
    })
}
