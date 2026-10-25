//! The minimal WSGI bridge for the spike (PEP 3333, design specification section 6.4).
//!
//! A request runs on one application thread. The environ is built from the
//! request head, `wsgi.input` reads body frames from a channel, and the
//! response head and body chunks go to the coalescing task through another
//! channel. The thread detaches from the interpreter only when it has to
//! wait: for a body frame, or for room in the response channel. Each wait
//! races against the request's interrupt, so the soft timeout (section
//! 7.6) wakes a thread blocked in the bridge at once.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use muru_proto::worker::v1 as pb;
use pyo3::exceptions::{
    PyBrokenPipeError, PyRuntimeError, PyStopIteration, PyTypeError, PyValueError,
};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyString, PyTuple};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};

use crate::pool::{Interrupt, Shared};

/// One item of application output on its way to the coalescing task.
pub enum Outgoing {
    /// The response head, sent once before any body.
    Head {
        /// HTTP status code.
        status: u32,
        /// The reason phrase the application gave, possibly empty.
        reason: String,
        /// Ordered header pairs as the application gave them, latin-1 encoded.
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        /// Flush policy derived from the headers.
        flush: pb::FlushPolicy,
    },
    /// One body chunk.
    Body(Bytes),
    /// The body, or its next piece, is a range of a file the edge sends
    /// itself (section 5.7): the descriptor goes on a FILE frame.
    File {
        /// A duplicate of the application's descriptor, closed once sent.
        fd: std::os::fd::OwnedFd,
        /// Where the range starts.
        offset: u64,
        /// How long it is.
        length: u64,
        /// Body chunks follow it rather than ending the response.
        more: bool,
    },
    /// The application failed: before the head, in which case the edge
    /// renders the error page from the reason and detail (section 4.12),
    /// or after it, in which case the stream ends with the error.
    Abort {
        /// What went wrong, in a sentence.
        error: String,
        /// The Appendix C.3 reason, app_error for an exception.
        reason: String,
        /// The traceback, for development mode.
        detail: String,
    },
    /// The application has started reading the request body (section 5.5).
    Continue,
    /// One whole WebSocket message, after a 101 head (section 5.6).
    Message {
        /// The message.
        data: Bytes,
        /// Text rather than binary.
        text: bool,
    },
    /// The application closes the WebSocket (section 5.6).
    Close {
        /// The close code.
        code: u16,
        /// The reason, possibly empty.
        reason: String,
    },
    /// The request is over: the application returned and its iterable was
    /// closed. Sent explicitly, because the `start_response` object that
    /// also holds a sender can outlive the request when a framework keeps
    /// an exception, and with it the frames that reference it, alive
    /// until a later collection.
    Finished,
}

/// Sends output to the coalescing task, detaching from the interpreter only
/// when the channel is full.
struct Sink {
    tx: mpsc::Sender<Outgoing>,
    interrupt: Arc<Interrupt>,
}

impl Sink {
    /// Send an item, waiting for room. An interrupted wait drops the item
    /// and reports success, so the thread returns to Python, where the
    /// timeout exception is waiting.
    fn send(&self, py: Python<'_>, item: Outgoing) -> bool {
        match self.tx.try_send(item) {
            Ok(()) => true,
            Err(TrySendError::Full(item)) => {
                let tx = &self.tx;
                let interrupt = &self.interrupt;
                py.detach(|| match interrupt.block_on(tx.send(item)) {
                    Some(sent) => sent.is_ok(),
                    None => true,
                })
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }
}

struct HeadState {
    status: Option<(u32, String)>,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    sent: bool,
}

/// The `start_response` callable handed to the application, which also
/// provides the `write` callable it returns.
#[pyclass(frozen, module = "muru._native")]
pub struct StartResponse {
    state: Mutex<HeadState>,
    sink: Sink,
    /// The worker's shared state and the request's id in it, so a
    /// streaming response can be exempted from the timeouts.
    shared: Arc<Shared>,
    request: u64,
}

impl StartResponse {
    fn new(
        tx: mpsc::Sender<Outgoing>,
        shared: Arc<Shared>,
        request: u64,
        interrupt: Arc<Interrupt>,
    ) -> Self {
        Self {
            state: Mutex::new(HeadState {
                status: None,
                headers: Vec::new(),
                sent: false,
            }),
            sink: Sink { tx, interrupt },
            shared,
            request,
        }
    }

    fn head_sent(&self) -> bool {
        self.state.lock().expect("head state poisoned").sent
    }

    /// Send the head if it has not gone yet. Fails if start_response was never called.
    fn ensure_head_sent(&self, py: Python<'_>) -> PyResult<()> {
        let mut state = self.state.lock().expect("head state poisoned");

        if state.sent {
            return Ok(());
        }

        let (status, reason) = state.status.take().ok_or_else(|| {
            PyRuntimeError::new_err("the application produced output before calling start_response")
        })?;
        state.sent = true;
        let headers = std::mem::take(&mut state.headers);
        drop(state);
        let flush = flush_policy(&headers);

        // Section 7.6: a response that streams runs as long as it needs to.

        if flush == pb::FlushPolicy::Immediate {
            self.shared.request_exempt(self.request);
        }

        if self.sink.send(
            py,
            Outgoing::Head {
                status,
                reason,
                headers,
                flush,
            },
        ) {
            Ok(())
        } else {
            Err(PyBrokenPipeError::new_err("the client went away"))
        }
    }

    fn send_body(&self, py: Python<'_>, data: Bytes) -> PyResult<()> {
        self.ensure_head_sent(py)?;

        if self.sink.send(py, Outgoing::Body(data)) {
            Ok(())
        } else {
            Err(PyBrokenPipeError::new_err("the client went away"))
        }
    }

    /// The Content-Length the application declared, while the head is
    /// still to be sent.
    fn declared_length(&self) -> Option<u64> {
        let state = self.state.lock().expect("head state poisoned");

        if state.sent {
            return None;
        }

        state
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(b"content-length"))
            .and_then(|(_, value)| std::str::from_utf8(value).ok()?.trim().parse().ok())
    }

    /// Hand the whole body to the edge as a range of a file (section 5.7).
    fn send_file(
        &self,
        py: Python<'_>,
        fd: std::os::fd::OwnedFd,
        offset: u64,
        length: u64,
    ) -> PyResult<()> {
        self.ensure_head_sent(py)?;

        if self.sink.send(
            py,
            Outgoing::File {
                fd,
                offset,
                length,
                more: false,
            },
        ) {
            Ok(())
        } else {
            Err(PyBrokenPipeError::new_err("the client went away"))
        }
    }

    /// The application failed before any response head: the edge renders
    /// the error page for the reason, with the traceback in development
    /// mode.
    fn fail(&self, py: Python<'_>, message: &str, reason: &str, detail: String) {
        self.state.lock().expect("head state poisoned").sent = true;
        self.abort(py, message, reason, detail);
    }

    /// The application failed after start_response: the edge reports the
    /// error if no output had reached it yet, and truncates the response
    /// otherwise.
    fn abort(&self, py: Python<'_>, message: &str, reason: &str, detail: String) {
        self.sink.send(
            py,
            Outgoing::Abort {
                error: message.to_owned(),
                reason: reason.to_owned(),
                detail,
            },
        );
    }
}

#[pymethods]
impl StartResponse {
    /// PEP 3333 `start_response(status, headers, exc_info=None)`.
    ///
    /// Validation follows mod_wsgi: the status must be three digits, a
    /// space and a reason without control characters; headers must be a
    /// list of (str, str) tuples with token names, no CR or LF in values,
    /// and latin-1 encodable. A second call before any output has been
    /// sent replaces the first, with or without exc_info, which PEP 3333
    /// forbids but every application that recovers from an error relies
    /// on. After output has started, exc_info is re-raised and a call
    /// without it is an error.
    #[pyo3(signature = (status, headers, exc_info=None))]
    fn __call__<'py>(
        slf: &Bound<'py, Self>,
        status: &str,
        headers: &Bound<'py, PyAny>,
        exc_info: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let this = slf.get();

        if let Some(info) = &exc_info {
            let tuple = info.cast::<PyTuple>().ok().filter(|tuple| tuple.len() == 3);

            if tuple.is_none() {
                return Err(PyTypeError::new_err(
                    "start_response() exc_info must be a 3-tuple or None",
                ));
            }
        }

        if this.head_sent() {
            return match exc_info {
                Some(info) => Err(PyErr::from_value(info.get_item(1)?)),
                None => Err(PyRuntimeError::new_err(
                    "start_response() called again after the response started",
                )),
            };
        }

        let (code, reason) = parse_status(status)?;
        let headers = validate_headers(py, headers)?;

        let mut state = this.state.lock().expect("head state poisoned");
        state.status = Some((code, reason));
        state.headers = headers;
        drop(state);

        slf.getattr(intern!(py, "write"))
    }

    /// The `write` callable of PEP 3333: send one chunk immediately.
    fn write(&self, py: Python<'_>, data: &Bound<'_, PyBytes>) -> PyResult<()> {
        self.send_body(py, bytes_from_py(data))
    }
}

/// Check and latin-1 encode the headers given to start_response.
fn validate_headers(
    py: Python<'_>,
    headers: &Bound<'_, PyAny>,
) -> PyResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let Ok(list) = headers.cast::<PyList>() else {
        return Err(PyTypeError::new_err(format!(
            "start_response() headers must be a list, not {}",
            type_name(headers)
        )));
    };

    let mut out = Vec::with_capacity(list.len());

    for item in list.iter() {
        let Ok(pair) = item.cast::<PyTuple>() else {
            return Err(PyTypeError::new_err(format!(
                "each header must be a (name, value) tuple, not {}",
                type_name(&item)
            )));
        };

        if pair.len() != 2 {
            return Err(PyValueError::new_err(format!(
                "each header must be a (name, value) tuple, got length {}",
                pair.len()
            )));
        }

        let name_obj = pair.get_item(0)?;
        let value_obj = pair.get_item(1)?;
        let Ok(name) = name_obj.cast::<PyString>() else {
            return Err(PyTypeError::new_err(format!(
                "header name must be str, not {}",
                type_name(&name_obj)
            )));
        };
        let Ok(value) = value_obj.cast::<PyString>() else {
            return Err(PyTypeError::new_err(format!(
                "header value must be str, not {}",
                type_name(&value_obj)
            )));
        };

        let name = latin1_encode(py, name)?;

        if name.is_empty() {
            return Err(PyValueError::new_err("header name is empty"));
        }

        if name.iter().any(|&byte| byte <= b' ' || byte == 0x7f) {
            return Err(PyValueError::new_err(format!(
                "header name contains a space or control character: {:?}",
                String::from_utf8_lossy(&name)
            )));
        }

        let value = latin1_encode(py, value)?;

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

/// Encode a native string as latin-1, raising UnicodeEncodeError as Python would.
fn latin1_encode(py: Python<'_>, text: &Bound<'_, PyString>) -> PyResult<Vec<u8>> {
    let encoded = text.call_method1(intern!(py, "encode"), (intern!(py, "latin-1"),))?;
    Ok(encoded.cast::<PyBytes>()?.as_bytes().to_vec())
}

fn type_name(object: &Bound<'_, PyAny>) -> String {
    object
        .get_type()
        .name()
        .map(|name| name.to_string())
        .unwrap_or_else(|_| "object".to_owned())
}

/// Chunks up to this size are copied; larger ones are shared with the Python object.
const COPY_LIMIT: usize = 4096;

/// A Python bytes object serving as the backing store of a `Bytes`.
///
/// Bytes objects are immutable, so their buffer can be read from any thread
/// while a reference is held. The reference is released when the last
/// `Bytes` clone drops; if that happens on a thread that is not attached to
/// the interpreter, PyO3 defers the release until one attaches.
struct PyBytesOwner(Py<PyBytes>);

impl AsRef<[u8]> for PyBytesOwner {
    fn as_ref(&self) -> &[u8] {
        let object = self.0.as_ptr();

        // SAFETY: the object is a bytes object, checked when the owner was
        // created, so these calls cannot fail; it is immutable and kept alive
        // by the reference in `self`, so the buffer stays valid and unchanged
        // for as long as the slice is borrowed.

        unsafe {
            let data = pyo3::ffi::PyBytes_AsString(object).cast::<u8>();
            let len = pyo3::ffi::PyBytes_Size(object);
            std::slice::from_raw_parts(data, usize::try_from(len).unwrap_or(0))
        }
    }
}

/// Turn a Python bytes object into `Bytes`, without copying large ones.
pub(crate) fn bytes_from_py(chunk: &Bound<'_, PyBytes>) -> Bytes {
    let data = chunk.as_bytes();

    if data.len() <= COPY_LIMIT {
        Bytes::copy_from_slice(data)
    } else {
        Bytes::from_owner(PyBytesOwner(chunk.clone().unbind()))
    }
}

/// Split a WSGI status into its code and reason phrase, rejecting
/// anything that is not three digits, a space and a reason free of
/// control characters.
fn parse_status(status: &str) -> PyResult<(u32, String)> {
    let invalid = || {
        PyValueError::new_err(format!(
            "invalid WSGI status {status:?}: expected three digits, a space and a reason"
        ))
    };
    let bytes = status.as_bytes();

    if bytes.len() < 4 || !bytes[..3].iter().all(u8::is_ascii_digit) || bytes[3] != b' ' {
        return Err(invalid());
    }

    let code: u32 = status[..3].parse().map_err(|_| invalid())?;

    if !(100..600).contains(&code) {
        return Err(invalid());
    }

    let reason = &status[4..];

    if reason.chars().any(|c| c.is_control()) {
        return Err(PyValueError::new_err(format!(
            "control character in status reason phrase: {status:?}"
        )));
    }

    Ok((code, reason.to_owned()))
}

/// Responses that must not be buffered flush immediately (section 5.4).
pub(crate) fn flush_policy(headers: &[(Vec<u8>, Vec<u8>)]) -> pb::FlushPolicy {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b"content-type")
            && value
                .get(..17)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"text/event-stream"))
        {
            return pb::FlushPolicy::Immediate;
        }

        if name.eq_ignore_ascii_case(b"x-accel-buffering") && value.eq_ignore_ascii_case(b"no") {
            return pb::FlushPolicy::Immediate;
        }
    }

    pb::FlushPolicy::Unspecified
}

struct InputState {
    rx: mpsc::Receiver<Bytes>,
    pending: Bytes,
    eof: bool,
    /// Where to announce the first read, when the client is waiting for
    /// 100 Continue before it sends the body (section 5.5).
    announce: Option<mpsc::Sender<Outgoing>>,
    /// The soft timeout's way out of a wait for the body.
    interrupt: Arc<Interrupt>,
}

impl InputState {
    /// Make sure some data is pending unless the body has ended. An
    /// interrupted wait ends the body, so the thread returns to Python,
    /// where the timeout exception is waiting.
    fn fill(&mut self, py: Python<'_>) -> bool {
        if !self.pending.is_empty() {
            return true;
        }

        if self.eof {
            return false;
        }

        if let Some(tx) = self.announce.take()
            && let Err(TrySendError::Full(item)) = tx.try_send(Outgoing::Continue)
        {
            let interrupt = &self.interrupt;
            py.detach(|| {
                let _ = interrupt.block_on(tx.send(item));
            });
        }

        let next = match self.rx.try_recv() {
            Ok(data) => Some(data),
            Err(TryRecvError::Empty) => {
                let rx = &mut self.rx;
                let interrupt = &self.interrupt;
                py.detach(|| interrupt.block_on(rx.recv()).flatten())
            }
            Err(TryRecvError::Disconnected) => None,
        };

        match next {
            Some(data) if !data.is_empty() => {
                self.pending = data;
                true
            }
            Some(_) => self.fill(py),
            None => {
                self.eof = true;
                false
            }
        }
    }

    fn read(&mut self, py: Python<'_>, size: Option<usize>) -> Vec<u8> {
        let mut out = Vec::new();

        loop {
            if size.is_some_and(|size| out.len() >= size) || !self.fill(py) {
                break;
            }

            let want = size
                .map_or(self.pending.len(), |size| size - out.len())
                .min(self.pending.len());
            out.extend_from_slice(&self.pending.split_to(want));
        }

        out
    }

    fn readline(&mut self, py: Python<'_>, limit: Option<usize>) -> Vec<u8> {
        let mut out = Vec::new();

        loop {
            if limit.is_some_and(|limit| out.len() >= limit) || !self.fill(py) {
                break;
            }

            let max = limit
                .map_or(usize::MAX, |limit| limit - out.len())
                .min(self.pending.len());

            match self.pending[..max].iter().position(|&byte| byte == b'\n') {
                Some(index) => {
                    out.extend_from_slice(&self.pending.split_to(index + 1));
                    break;
                }
                None => out.extend_from_slice(&self.pending.split_to(max)),
            }
        }

        out
    }
}

/// The `wsgi.input` stream: body frames from the edge, read on demand.
#[pyclass(frozen, module = "muru._native")]
pub struct Input {
    state: Mutex<InputState>,
}

impl Input {
    fn new(
        rx: mpsc::Receiver<Bytes>,
        announce: Option<mpsc::Sender<Outgoing>>,
        interrupt: Arc<Interrupt>,
    ) -> Self {
        Self {
            state: Mutex::new(InputState {
                rx,
                pending: Bytes::new(),
                eof: false,
                announce,
                interrupt,
            }),
        }
    }
}

fn optional_size(value: i64) -> Option<usize> {
    usize::try_from(value).ok()
}

#[pymethods]
impl Input {
    /// Read up to `size` bytes, or everything that remains when `size` is negative.
    #[pyo3(signature = (size=-1))]
    fn read<'py>(&self, py: Python<'py>, size: i64) -> Bound<'py, PyBytes> {
        let data = self
            .state
            .lock()
            .expect("input state poisoned")
            .read(py, optional_size(size));
        PyBytes::new(py, &data)
    }

    /// Read one line, up to `limit` bytes when `limit` is not negative.
    #[pyo3(signature = (limit=-1))]
    fn readline<'py>(&self, py: Python<'py>, limit: i64) -> Bound<'py, PyBytes> {
        let data = self
            .state
            .lock()
            .expect("input state poisoned")
            .readline(py, optional_size(limit));
        PyBytes::new(py, &data)
    }

    /// Read lines until the body ends, or until `hint` bytes have been read.
    #[pyo3(signature = (hint=-1))]
    fn readlines<'py>(&self, py: Python<'py>, hint: i64) -> Vec<Bound<'py, PyBytes>> {
        let hint = optional_size(hint).filter(|&hint| hint > 0);
        let mut lines = Vec::new();
        let mut total = 0;
        let mut state = self.state.lock().expect("input state poisoned");

        loop {
            let line = state.readline(py, None);

            if line.is_empty() {
                break;
            }

            total += line.len();
            lines.push(PyBytes::new(py, &line));

            if hint.is_some_and(|hint| total >= hint) {
                break;
            }
        }

        lines
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let line = self
            .state
            .lock()
            .expect("input state poisoned")
            .readline(py, None);

        if line.is_empty() {
            Err(PyStopIteration::new_err(()))
        } else {
            Ok(PyBytes::new(py, &line))
        }
    }

    /// Closing the request body is a no-op; the edge owns the connection.
    fn close(&self) {}
}

/// Decode bytes as ISO-8859-1, the encoding PEP 3333 prescribes for native strings.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| byte as char).collect()
}

/// Decode `%XX` escapes. Other bytes, including `+`, are left alone.
pub(crate) fn percent_decode(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0;

    while index < raw.len() {
        if raw[index] == b'%' && index + 2 < raw.len() {
            let hex = &raw[index + 1..index + 3];

            if let Ok(byte) = u8::from_str_radix(std::str::from_utf8(hex).unwrap_or("zz"), 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }

        out.push(raw[index]);
        index += 1;
    }

    out
}

fn environ_key(name: &[u8]) -> String {
    if name.eq_ignore_ascii_case(b"content-type") {
        return "CONTENT_TYPE".to_owned();
    }

    if name.eq_ignore_ascii_case(b"content-length") {
        return "CONTENT_LENGTH".to_owned();
    }

    let mut key = String::with_capacity(5 + name.len());
    key.push_str("HTTP_");

    for &byte in name {
        key.push(if byte == b'-' {
            '_'
        } else {
            byte.to_ascii_uppercase() as char
        });
    }

    key
}

fn build_environ<'py>(
    py: Python<'py>,
    shared: &Shared,
    head: &pb::RequestHead,
    input: Bound<'py, Input>,
) -> PyResult<Bound<'py, PyDict>> {
    let environ = PyDict::new(py);
    environ.set_item(intern!(py, "REQUEST_METHOD"), &head.method)?;
    let path = latin1(&percent_decode(&head.raw_path));
    let path_info = path
        .strip_prefix(head.script_name.as_str())
        .unwrap_or(&path);
    environ.set_item(intern!(py, "SCRIPT_NAME"), &head.script_name)?;
    environ.set_item(intern!(py, "PATH_INFO"), path_info)?;
    environ.set_item(intern!(py, "QUERY_STRING"), latin1(&head.query))?;

    let protocol = if head.http_version.is_empty() {
        "HTTP/1.1"
    } else {
        head.http_version.as_str()
    };
    environ.set_item(intern!(py, "SERVER_PROTOCOL"), protocol)?;

    // Section 4.3: SERVER_NAME is the virtual host matched, else the
    // listener's address.

    match &head.server {
        Some(server) => {
            let name = if head.server_name.is_empty() {
                server.address.as_str()
            } else {
                head.server_name.as_str()
            };
            environ.set_item(intern!(py, "SERVER_NAME"), name)?;
            environ.set_item(intern!(py, "SERVER_PORT"), server.port.to_string())?;
        }
        None => {
            environ.set_item(intern!(py, "SERVER_NAME"), "localhost")?;
            environ.set_item(intern!(py, "SERVER_PORT"), "80")?;
        }
    }

    if let Some(client) = &head.client {
        environ.set_item(intern!(py, "REMOTE_ADDR"), &client.address)?;
        environ.set_item(intern!(py, "REMOTE_PORT"), client.port.to_string())?;
    }

    for header in &head.headers {
        let key = environ_key(&header.name);
        let value = latin1(&header.value);

        match environ.get_item(&key)? {
            Some(existing) => {
                let joined = format!("{}, {}", existing.extract::<String>()?, value);
                environ.set_item(key, joined)?;
            }
            None => environ.set_item(key, value)?,
        }
    }

    // A body the edge buffered has a length even when the client sent
    // none (chunked_body = buffer in section 4.6).

    if let Some(length) = head.content_length
        && environ.get_item(intern!(py, "CONTENT_LENGTH"))?.is_none()
    {
        environ.set_item(intern!(py, "CONTENT_LENGTH"), length.to_string())?;
    }
    environ.set_item(intern!(py, "wsgi.version"), (1, 0))?;

    let scheme = if head.scheme.is_empty() {
        "http"
    } else {
        head.scheme.as_str()
    };
    environ.set_item(intern!(py, "wsgi.url_scheme"), scheme)?;

    if scheme == "https" {
        environ.set_item(intern!(py, "HTTPS"), "on")?;
    }

    if let Some(tls) = &head.tls {
        let details = PyDict::new(py);
        details.set_item(intern!(py, "version"), &tls.version)?;
        details.set_item(intern!(py, "cipher"), &tls.cipher)?;
        details.set_item(intern!(py, "server_name"), &tls.server_name)?;
        details.set_item(intern!(py, "alpn"), &tls.alpn)?;
        environ.set_item(intern!(py, "muru.tls"), details)?;
    }

    environ.set_item(intern!(py, "wsgi.input"), input)?;
    environ.set_item(intern!(py, "wsgi.errors"), shared.errors.bind(py))?;
    environ.set_item(intern!(py, "wsgi.multithread"), shared.threads > 1)?;
    environ.set_item(intern!(py, "wsgi.multiprocess"), shared.multiprocess)?;
    environ.set_item(intern!(py, "wsgi.run_once"), false)?;
    environ.set_item(intern!(py, "wsgi.input_terminated"), true)?;
    environ.set_item(
        intern!(py, "wsgi.file_wrapper"),
        py.get_type::<FileWrapper>(),
    )?;
    environ.set_item(intern!(py, "muru.request_id"), &head.request_id)?;

    if head.max_body_size != 0 {
        environ.set_item(intern!(py, "muru.max_body_size"), head.max_body_size)?;
    }

    // Section 4.10: a request that arrived by internal redirect carries
    // the CGI REDIRECT_* variables for what the client asked for.

    if let Some(internal) = &head.internal {
        environ.set_item(intern!(py, "muru.internal"), true)?;

        // Section 4.16: an authentication sub-request describes the
        // request it decides about instead.

        if internal.auth_request {
            environ.set_item(intern!(py, "muru.auth_request"), true)?;
            environ.set_item(
                intern!(py, "muru.original_method"),
                &internal.original_method,
            )?;
            environ.set_item(intern!(py, "muru.original_uri"), &internal.original_uri)?;
            environ.set_item(intern!(py, "muru.original_host"), &internal.original_host)?;
        } else {
            environ.set_item(intern!(py, "REDIRECT_URL"), &internal.redirect_url)?;
            environ.set_item(
                intern!(py, "REDIRECT_QUERY_STRING"),
                &internal.redirect_query_string,
            )?;
            environ.set_item(
                intern!(py, "REDIRECT_STATUS"),
                internal.redirect_status.to_string(),
            )?;
        }

        // Section 4.12: an error page sub-request says which error.

        if !internal.error_reason.is_empty() {
            environ.set_item(intern!(py, "muru.error_reason"), &internal.error_reason)?;
        }
    }

    // Section 4.7: behind a trusted proxy, REMOTE_ADDR is the resolved
    // client and the peer that actually connected is kept beside it.

    if let Some(proxy) = &head.proxy {
        environ.set_item(intern!(py, "muru.proxy_addr"), &proxy.address)?;
    }

    Ok(environ)
}

/// Run one request through the application on the current, attached
/// thread. `request` is the request's id in the shared state and
/// `interrupt` the soft timeout's way out of the bridge's waits.
#[allow(clippy::too_many_arguments)] // one call site; each argument is a distinct part of the request
pub fn run(
    py: Python<'_>,
    shared: &Arc<Shared>,
    request: u64,
    app: &Py<PyAny>,
    head: pb::RequestHead,
    body_rx: mpsc::Receiver<Bytes>,
    tx: mpsc::Sender<Outgoing>,
    cancelled: Arc<AtomicBool>,
    interrupt: Arc<Interrupt>,
) {
    let announce = head.expect_continue.then(|| tx.clone());
    let finished = tx.clone();
    let started = StartResponse::new(tx, Arc::clone(shared), request, Arc::clone(&interrupt));

    let start_response = match Bound::new(py, started) {
        Ok(start_response) => start_response,
        Err(error) => {
            error.print(py);
            return;
        }
    };

    if let Err(error) = run_app(
        py,
        shared,
        app,
        &head,
        body_rx,
        announce,
        &start_response,
        &cancelled,
        Arc::clone(&interrupt),
    ) {
        let broken_pipe = error.is_instance_of::<PyBrokenPipeError>(py);

        if !broken_pipe {
            report(py, &error);
        }

        // Section 7.6: the timeout exception, raised in this thread by
        // the watchdog, is a 504 rather than a 500.

        let timed_out = shared
            .soft_timeout
            .as_ref()
            .filter(|timeout| error.is_instance(py, timeout.error.bind(py)));

        let (message, reason) = match timed_out {
            Some(timeout) => (
                format!(
                    "the request ran past the soft timeout of {}",
                    describe_seconds(timeout.after)
                ),
                "soft_timeout",
            ),
            None => (
                "the application raised an exception".to_owned(),
                "app_error",
            ),
        };

        let start_response = start_response.get();

        if !start_response.head_sent() {
            start_response.fail(py, &message, reason, describe(py, &error));
        } else if !broken_pipe {
            start_response.abort(py, &message, reason, describe(py, &error));
        }
    }
    let sink = Sink {
        tx: finished,
        interrupt,
    };
    sink.send(py, Outgoing::Finished);
}

/// A duration in seconds, as the log shows it: `2 s` or `1.5 s`.
pub(crate) fn describe_seconds(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs_f64();

    if seconds.fract() == 0.0 {
        format!("{seconds:.0} s")
    } else {
        format!("{seconds} s")
    }
}

#[allow(clippy::too_many_arguments)] // one call site; each argument is a distinct part of the request
fn run_app<'py>(
    py: Python<'py>,
    shared: &Shared,
    app: &Py<PyAny>,
    head: &pb::RequestHead,
    body_rx: mpsc::Receiver<Bytes>,
    announce: Option<mpsc::Sender<Outgoing>>,
    start_response: &Bound<'py, StartResponse>,
    cancelled: &AtomicBool,
    interrupt: Arc<Interrupt>,
) -> PyResult<()> {
    let input = Bound::new(py, Input::new(body_rx, announce, interrupt))?;
    let environ = build_environ(py, shared, head, input)?;
    let result = app.bind(py).call1((environ, start_response))?;

    // Section 6.5: a file the edge can send itself goes by descriptor;
    // anything else is iterated.

    let outcome = match file_range(py, &result, start_response.get()) {
        Some((fd, offset, length)) => start_response.get().send_file(py, fd, offset, length),
        None => iterate(py, &result, start_response.get(), cancelled),
    };

    // PEP 3333: close() is called whether or not the iteration completed.
    // An error from close() is reported but does not fail a response that
    // has been fully produced.

    if let Ok(close) = result.getattr(intern!(py, "close"))
        && let Err(error) = close.call0()
    {
        report(py, &error);
    }
    outcome
}

/// Print an exception with its traceback to stderr, the worker's log.
///
/// `PyErr::print` is not used because it exits the process for SystemExit,
/// and an application raising SystemExit is a failed request, not a reason
/// to stop serving.
/// Format an exception with its traceback, as the traceback module does.
pub(crate) fn describe(py: Python<'_>, error: &PyErr) -> String {
    let formatted = py
        .import(intern!(py, "traceback"))
        .and_then(|module| module.call_method1(intern!(py, "format_exception"), (error.value(py),)))
        .and_then(|lines| lines.extract::<Vec<String>>());

    match formatted {
        Ok(lines) => lines.concat(),
        Err(_) => error.to_string(),
    }
}

pub(crate) fn report(py: Python<'_>, error: &PyErr) {
    let printed = py
        .import(intern!(py, "traceback"))
        .and_then(|module| module.call_method1(intern!(py, "print_exception"), (error.value(py),)));

    if printed.is_err() {
        eprintln!("muru: application error: {error}");
    }

    let _ = py
        .import(intern!(py, "sys"))
        .and_then(|sys| sys.getattr(intern!(py, "stderr")))
        .and_then(|stderr| stderr.call_method0(intern!(py, "flush")));
}

fn iterate(
    py: Python<'_>,
    result: &Bound<'_, PyAny>,
    start_response: &StartResponse,
    cancelled: &AtomicBool,
) -> PyResult<()> {
    for item in result.try_iter()? {
        let item = item?;
        let chunk = item.cast::<PyBytes>().map_err(|_| {
            PyTypeError::new_err("the application yielded an object that is not bytes")
        })?;

        if !chunk.as_bytes().is_empty() {
            start_response.send_body(py, bytes_from_py(chunk))?;
        }

        if cancelled.load(Ordering::Relaxed) {
            break;
        }
    }

    start_response.ensure_head_sent(py)
}

/// Section 6.5: the range of a regular file the edge sends in place of
/// iterating the response, when the application returned the object from
/// `wsgi.file_wrapper` itself and nothing has been written yet. The range
/// starts at the file's position and covers the declared Content-Length,
/// or the rest of the file. None means the response is iterated as PEP
/// 3333 describes.
fn file_range(
    py: Python<'_>,
    result: &Bound<'_, PyAny>,
    start_response: &StartResponse,
) -> Option<(std::os::fd::OwnedFd, u64, u64)> {
    let wrapper = result.cast::<FileWrapper>().ok()?;

    if start_response.head_sent() {
        return None;
    }

    let (fd, position, size) = wrapper.get().descriptor(py)?;
    let length = start_response
        .declared_length()
        .unwrap_or_else(|| size.saturating_sub(position))
        .min(size.saturating_sub(position));

    if length == 0 {
        return None;
    }

    Some((fd, position, length))
}

/// `wsgi.file_wrapper`: iterate a file-like object in blocks, and close it.
///
/// Returned as the response iterable itself, with a regular file behind
/// it, the bridge passes a duplicate of its descriptor to the edge instead
/// (section 5.7); iteration is the fallback for everything else.
#[pyclass(frozen, module = "muru._native")]
pub struct FileWrapper {
    file: Py<PyAny>,
    block_size: usize,
}

impl FileWrapper {
    /// A duplicate of the file's descriptor with its position and size,
    /// when it is a regular file.
    fn descriptor(&self, py: Python<'_>) -> Option<(std::os::fd::OwnedFd, u64, u64)> {
        use std::os::fd::{AsRawFd, BorrowedFd};

        let file = self.file.bind(py);
        let raw = file
            .call_method0(intern!(py, "fileno"))
            .ok()?
            .extract::<i32>()
            .ok()?;

        if raw < 0 {
            return None;
        }

        // SAFETY: the descriptor is the application's open file, alive for
        // the call since the application object holding it is.

        let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
        let copy = std::fs::File::from(borrowed.try_clone_to_owned().ok()?);
        let size = copy
            .metadata()
            .ok()
            .filter(|metadata| metadata.is_file())?
            .len();

        // The file's own position: Python buffers reads, so its tell() is
        // the truth, and the descriptor's offset is a fallback for a raw
        // file object.

        let position = file
            .call_method0(intern!(py, "tell"))
            .and_then(|value| value.extract::<u64>())
            .ok()
            .or_else(|| {
                // SAFETY: lseek on a descriptor this process owns.

                let offset = unsafe { libc::lseek(copy.as_raw_fd(), 0, libc::SEEK_CUR) };
                u64::try_from(offset).ok()
            })?;

        Some((copy.into(), position, size))
    }
}

#[pymethods]
impl FileWrapper {
    #[new]
    #[pyo3(signature = (filelike, block_size=8192))]
    fn new(filelike: Py<PyAny>, block_size: usize) -> Self {
        Self {
            file: filelike,
            block_size: block_size.max(1),
        }
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let block = self
            .file
            .bind(py)
            .call_method1(intern!(py, "read"), (self.block_size,))?;
        let Ok(block) = block.cast_into::<PyBytes>() else {
            return Err(PyTypeError::new_err(
                "wsgi.file_wrapper: read() did not return bytes",
            ));
        };

        if block.as_bytes().is_empty() {
            Err(PyStopIteration::new_err(()))
        } else {
            Ok(block)
        }
    }

    /// Close the wrapped file, if it can be closed.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let file = self.file.bind(py);

        if file.hasattr(intern!(py, "close"))? {
            file.call_method0(intern!(py, "close"))?;
        }

        Ok(())
    }
}
