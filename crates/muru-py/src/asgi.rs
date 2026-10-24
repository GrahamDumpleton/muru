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
    /// The WebSocket handshake has arrived (section 5.6).
    Connect,
    /// One WebSocket message from the client.
    Received { data: Bytes, text: bool },
    /// The client went away, or the response is complete. On a WebSocket
    /// the close code and reason say how it ended.
    Disconnect { code: u16, reason: String },
}

/// Messages waiting for the application, and the application waiting for
/// a message.
struct Inbox {
    queue: VecDeque<Message>,
    /// A future from `receive()` waiting for the next message.
    waiter: Option<Py<PyAny>>,
    /// No more body will arrive: every later `receive()` gets a disconnect.
    closed: bool,
    /// How a WebSocket ended, for the disconnect message.
    close: (u16, String),
}

/// What the application has sent so far.
#[derive(Default)]
struct SendState {
    head_sent: bool,
    body_ended: bool,
    /// The WebSocket is accepted: a 101 went out.
    accepted: bool,
    /// The application has closed the WebSocket.
    closed: bool,
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
    /// The request is a WebSocket upgrade (section 5.6).
    websocket: bool,
    /// Mirrors `SendState.accepted`, for the link's read side.
    accepted: AtomicBool,
}

impl Channel {
    fn new(
        py: Python<'_>,
        asgi: &Asgi,
        tx: mpsc::Sender<Outgoing>,
        head: &pb::RequestHead,
    ) -> Self {
        Self {
            inbox: Mutex::new(Inbox {
                queue: VecDeque::new(),
                waiter: None,
                closed: false,
                close: (1005, String::new()),
            }),
            state: Mutex::new(SendState::default()),
            tx,
            runtime: tokio::runtime::Handle::current(),
            event_loop: asgi.event_loop.clone_ref(py),
            resolver: asgi.resolver.clone_ref(py),
            permits: Arc::new(tokio::sync::Semaphore::new(INBOX_CAPACITY)),
            complete: tokio::sync::Notify::new(),
            completed: AtomicBool::new(false),
            announce: AtomicBool::new(head.expect_continue),
            websocket: head.scope == pb::Scope::Websocket as i32,
            accepted: AtomicBool::new(false),
        }
    }

    /// Whether the application has accepted the WebSocket.
    pub fn accepted(&self) -> bool {
        self.accepted.load(Ordering::Relaxed)
    }

    /// Hand the WebSocket's connect event to the application.
    pub async fn deliver_connect(&self) {
        self.deliver_message_inner(Message::Connect).await;
    }

    /// Hand one WebSocket message from the client to the application.
    pub async fn deliver_message(&self, data: Bytes, text: bool) {
        self.deliver_message_inner(Message::Received { data, text })
            .await;
    }

    async fn deliver_message_inner(&self, message: Message) {
        let Ok(permit) = self.permits.acquire().await else {
            return;
        };
        permit.forget();
        self.push(message);
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
        self.disconnect_with(1005, "");
    }

    /// The WebSocket has ended with a close code: the next `receive()` and
    /// every one after it gets `websocket.disconnect`.
    pub fn disconnect_with(&self, code: u16, reason: &str) {
        let mut inbox = self.inbox.lock().expect("inbox poisoned");

        if inbox.closed {
            return;
        }

        inbox.closed = true;
        inbox.close = (code, reason.to_owned());

        if let Some(waiter) = inbox.waiter.take() {
            drop(inbox);
            self.wake(waiter, &self.disconnect_message());
        }
    }

    fn disconnect_message(&self) -> Message {
        let (code, reason) = self.inbox.lock().expect("inbox poisoned").close.clone();
        Message::Disconnect { code, reason }
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
            let delivered = message_dict(py, message, self.websocket).and_then(|value| {
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

impl Channel {
    /// Handle the websocket.* messages. Returns None for a message that is
    /// not one of them, which the http handling then takes.
    fn send_websocket(
        &self,
        py: Python<'_>,
        kind: &str,
        message: &Bound<'_, PyDict>,
        state: &mut SendState,
    ) -> PyResult<Option<Option<Py<PyAny>>>> {
        match kind {
            "websocket.accept" => {
                if state.accepted || state.closed || state.head_sent {
                    return Err(PyRuntimeError::new_err(
                        "websocket.accept sent after the WebSocket was accepted or closed",
                    ));
                }

                // The subprotocol and any headers the application chose go
                // out with the 101.

                let mut headers = match message.get_item("headers")? {
                    Some(headers) if !headers.is_none() => validate_headers(&headers)?,
                    _ => Vec::new(),
                };

                if let Some(subprotocol) = message.get_item("subprotocol")?
                    && !subprotocol.is_none()
                {
                    let subprotocol = subprotocol.extract::<String>().map_err(|_| {
                        PyTypeError::new_err("websocket.accept subprotocol must be a str")
                    })?;
                    headers.push((b"sec-websocket-protocol".to_vec(), subprotocol.into_bytes()));
                }

                state.accepted = true;
                state.head_sent = true;
                self.accepted.store(true, Ordering::Relaxed);
                let head = Outgoing::Head {
                    status: 101,
                    reason: String::new(),
                    headers,
                    flush: pb::FlushPolicy::Immediate,
                };
                Ok(Some(self.enqueue(py, vec![head])?))
            }
            "websocket.send" => {
                if !state.accepted {
                    return Err(PyRuntimeError::new_err(
                        "websocket.send sent before websocket.accept",
                    ));
                }

                if state.closed {
                    return Err(PyRuntimeError::new_err(
                        "websocket.send sent after the WebSocket was closed",
                    ));
                }

                // Exactly one of bytes and text carries the message.

                let bytes = message.get_item("bytes")?.filter(|value| !value.is_none());
                let text = message.get_item("text")?.filter(|value| !value.is_none());

                let outgoing = match (bytes, text) {
                    (Some(bytes), None) => {
                        let Ok(bytes) = bytes.cast::<PyBytes>() else {
                            return Err(PyTypeError::new_err("websocket.send bytes must be bytes"));
                        };
                        Outgoing::Message {
                            data: wsgi::bytes_from_py(bytes),
                            text: false,
                        }
                    }
                    (None, Some(text)) => {
                        let text = text.extract::<String>().map_err(|_| {
                            PyTypeError::new_err("websocket.send text must be a str")
                        })?;
                        Outgoing::Message {
                            data: Bytes::from(text),
                            text: true,
                        }
                    }
                    _ => {
                        return Err(PyValueError::new_err(
                            "websocket.send needs exactly one of bytes and text",
                        ));
                    }
                };

                Ok(Some(self.enqueue(py, vec![outgoing])?))
            }
            "websocket.close" => {
                if state.closed || state.body_ended {
                    return Err(PyRuntimeError::new_err(
                        "websocket.close sent after the WebSocket was closed",
                    ));
                }

                let code = match message.get_item("code")? {
                    Some(code) if !code.is_none() => code
                        .extract::<u16>()
                        .map_err(|_| PyTypeError::new_err("websocket.close code must be an int"))?,
                    _ => 1000,
                };

                let reason = match message.get_item("reason")? {
                    Some(reason) if !reason.is_none() => {
                        reason.extract::<String>().map_err(|_| {
                            PyTypeError::new_err("websocket.close reason must be a str")
                        })?
                    }
                    _ => String::new(),
                };

                state.closed = true;

                // Closing before accepting declines the handshake with 403.

                if !state.accepted {
                    state.head_sent = true;
                    state.body_ended = true;
                    return Ok(Some(self.enqueue(
                        py,
                        vec![
                            Outgoing::Head {
                                status: 403,
                                reason: String::new(),
                                headers: vec![(b"content-length".to_vec(), b"0".to_vec())],
                                flush: pb::FlushPolicy::Unspecified,
                            },
                            Outgoing::Finished,
                        ],
                    )?));
                }

                Ok(Some(
                    self.enqueue(py, vec![Outgoing::Close { code, reason }])?,
                ))
            }
            "websocket.http.response.start" | "websocket.http.response.body" => {
                if state.accepted {
                    return Err(PyRuntimeError::new_err(format!(
                        "{kind} sent after the WebSocket was accepted"
                    )));
                }

                // The websocket.http.response extension: an ordinary
                // response declines the handshake.

                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Answer the upgrade with an empty response of this status.
    fn decline(&self, py: Python<'_>, status: u32) {
        let mut state = self.state.lock().expect("send state poisoned");
        state.head_sent = true;
        state.body_ended = true;
        state.closed = true;
        drop(state);

        let _ = self.enqueue(
            py,
            vec![
                Outgoing::Head {
                    status,
                    reason: String::new(),
                    headers: vec![(b"content-length".to_vec(), b"0".to_vec())],
                    flush: pb::FlushPolicy::Unspecified,
                },
                Outgoing::Finished,
            ],
        );
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
            future.call_method1("set_result", (message_dict(py, &message, self.websocket)?,))?;
        } else if inbox.closed {
            let (code, reason) = inbox.close.clone();
            drop(inbox);
            let message = Message::Disconnect { code, reason };
            future.call_method1("set_result", (message_dict(py, &message, self.websocket)?,))?;
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
        if self.websocket
            && let Some(result) = self.send_websocket(py, &kind, message, &mut state)?
        {
            return Ok(result);
        }
        let kind = kind.strip_prefix("websocket.").unwrap_or(&kind).to_owned();

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
    /// started, or never finished, is an error the edge reports; a
    /// WebSocket left open is closed, and one never accepted is declined.
    fn finish(&self, py: Python<'_>) {
        let state = self.state.lock().expect("send state poisoned");
        let (head_sent, body_ended) = (state.head_sent, state.body_ended);
        let (accepted, closed) = (state.accepted, state.closed);
        drop(state);

        if body_ended || closed {
            return;
        }

        if accepted {
            let _ = self.enqueue(
                py,
                vec![Outgoing::Close {
                    code: 1000,
                    reason: String::new(),
                }],
            );
            return;
        }

        if self.websocket && !head_sent {
            self.decline(py, 403);
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
        let state = self.state.lock().expect("send state poisoned");
        let (body_ended, accepted, closed) = (state.body_ended, state.accepted, state.closed);
        drop(state);

        if body_ended || closed {
            return;
        }

        // An accepted WebSocket ends with 1011; anything else is a 500.

        if accepted {
            let _ = self.enqueue(
                py,
                vec![Outgoing::Close {
                    code: 1011,
                    reason: "internal error".to_owned(),
                }],
            );
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
fn message_dict<'py>(
    py: Python<'py>,
    message: &Message,
    websocket: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);

    match message {
        Message::Body { data, more } => {
            dict.set_item("type", "http.request")?;
            dict.set_item("body", PyBytes::new(py, data))?;
            dict.set_item("more_body", *more)?;
        }
        Message::Connect => {
            dict.set_item("type", "websocket.connect")?;
        }
        Message::Received { data, text } => {
            dict.set_item("type", "websocket.receive")?;

            if *text {
                dict.set_item("text", String::from_utf8_lossy(data).as_ref())?;
                dict.set_item("bytes", py.None())?;
            } else {
                dict.set_item("bytes", PyBytes::new(py, data))?;
                dict.set_item("text", py.None())?;
            }
        }
        Message::Disconnect { code, reason } if websocket => {
            dict.set_item("type", "websocket.disconnect")?;
            dict.set_item("code", *code)?;
            dict.set_item("reason", reason)?;
        }
        Message::Disconnect { .. } => {
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
    let websocket = head.scope == pb::Scope::Websocket as i32;
    let scope = PyDict::new(py);
    scope.set_item("type", if websocket { "websocket" } else { "http" })?;
    let asgi = PyDict::new(py);
    asgi.set_item("version", "3.0")?;
    asgi.set_item("spec_version", "2.4")?;
    scope.set_item("asgi", asgi)?;

    // The request line, as ASGI wants it: the path decoded, the raw path
    // and query as bytes, the mount prefix as root_path.

    let version = match head.http_version.as_str() {
        "HTTP/2.0" | "HTTP/2" => "2",
        "HTTP/3.0" | "HTTP/3" => "3",
        "HTTP/1.0" => "1.0",
        _ => "1.1",
    };

    scope.set_item("http_version", version)?;

    let scheme = if head.scheme.is_empty() {
        "http"
    } else {
        head.scheme.as_str()
    };

    if websocket {
        scope.set_item("scheme", if scheme == "https" { "wss" } else { "ws" })?;
    } else {
        scope.set_item("method", &head.method)?;
        scope.set_item("scheme", scheme)?;
    }
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
    let mut subprotocols: Vec<String> = Vec::new();

    for header in &head.headers {
        has_length |= header.name.eq_ignore_ascii_case(b"content-length");

        if header.name.eq_ignore_ascii_case(b"sec-websocket-protocol") {
            let offered = String::from_utf8_lossy(&header.value);
            subprotocols.extend(offered.split(',').map(|name| name.trim().to_owned()));
        }

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

    if websocket {
        scope.set_item("subprotocols", subprotocols)?;
    }

    // The peers, and the state the runner fills from the lifespan.

    scope.set_item(
        "client",
        head.client
            .as_ref()
            .map(|peer| (peer.address.as_str(), peer.port)),
    )?;
    scope.set_item(
        "server",
        head.server.as_ref().map(|peer| {
            let name = if head.server_name.is_empty() {
                peer.address.as_str()
            } else {
                head.server_name.as_str()
            };
            (name, peer.port)
        }),
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

    if websocket {
        // Declining the handshake with a full response (section 5.6).

        extensions.set_item("websocket.http.response", PyDict::new(py))?;
    }

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
        let channel =
            Bound::new(py, Channel::new(py, asgi, tx, head)).map_err(|error| error.to_string())?;
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
