//! The minimal WSGI bridge for the spike (PEP 3333, design specification section 6.4).
//!
//! A request runs on one application thread. The environ is built from the
//! request head, `wsgi.input` reads body frames from a channel, and the
//! response head and body chunks go to the coalescing task through another
//! channel. The thread detaches from the interpreter only when it has to
//! wait: for a body frame, or for room in the response channel.

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
use pyo3::types::{PyBytes, PyDict};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};

use crate::pool::Shared;

/// One item of application output on its way to the coalescing task.
pub enum Outgoing {
    /// The response head, sent once before any body.
    Head {
        /// HTTP status code.
        status: u32,
        /// Ordered header pairs as the application gave them.
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        /// Flush policy derived from the headers.
        flush: pb::FlushPolicy,
    },
    /// One body chunk.
    Body(Bytes),
    /// The application failed after the head was sent; end the stream with an error.
    Abort(String),
}

/// Sends output to the coalescing task, detaching from the interpreter only
/// when the channel is full.
struct Sink {
    tx: mpsc::Sender<Outgoing>,
}

impl Sink {
    fn send(&self, py: Python<'_>, item: Outgoing) -> bool {
        match self.tx.try_send(item) {
            Ok(()) => true,
            Err(TrySendError::Full(item)) => {
                let tx = &self.tx;
                py.detach(|| tx.blocking_send(item).is_ok())
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }
}

struct HeadState {
    status: Option<u32>,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    sent: bool,
}

/// The `start_response` callable handed to the application, which also
/// provides the `write` callable it returns.
#[pyclass(frozen, module = "muru._native")]
pub struct StartResponse {
    state: Mutex<HeadState>,
    sink: Sink,
}

impl StartResponse {
    fn new(tx: mpsc::Sender<Outgoing>) -> Self {
        Self {
            state: Mutex::new(HeadState {
                status: None,
                headers: Vec::new(),
                sent: false,
            }),
            sink: Sink { tx },
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
        let status = state.status.ok_or_else(|| {
            PyRuntimeError::new_err("the application produced output before calling start_response")
        })?;
        state.sent = true;
        let headers = std::mem::take(&mut state.headers);
        drop(state);
        let flush = flush_policy(&headers);
        if self.sink.send(
            py,
            Outgoing::Head {
                status,
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

    /// Reply with a plain 500 when the application failed before any output.
    fn send_error_response(&self, py: Python<'_>) {
        let body = b"Internal Server Error\n";
        {
            let mut state = self.state.lock().expect("head state poisoned");
            state.sent = true;
        }
        let headers = vec![
            (
                b"content-type".to_vec(),
                b"text/plain; charset=utf-8".to_vec(),
            ),
            (
                b"content-length".to_vec(),
                body.len().to_string().into_bytes(),
            ),
        ];
        if self.sink.send(
            py,
            Outgoing::Head {
                status: 500,
                headers,
                flush: pb::FlushPolicy::Unspecified,
            },
        ) {
            self.sink.send(py, Outgoing::Body(Bytes::from_static(body)));
        }
    }

    fn abort(&self, py: Python<'_>, message: &str) {
        self.sink.send(py, Outgoing::Abort(message.to_owned()));
    }
}

#[pymethods]
impl StartResponse {
    #[pyo3(signature = (status, headers, exc_info=None))]
    fn __call__<'py>(
        slf: &Bound<'py, Self>,
        status: &str,
        headers: Vec<(String, String)>,
        exc_info: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let this = slf.get();
        {
            let mut state = this.state.lock().expect("head state poisoned");
            if state.sent {
                return match exc_info {
                    Some(info) => Err(PyErr::from_value(info.get_item(1)?)),
                    None => Err(PyRuntimeError::new_err(
                        "start_response called after the headers were sent",
                    )),
                };
            }
            if state.status.is_some() && exc_info.is_none() {
                return Err(PyRuntimeError::new_err(
                    "start_response called twice without exc_info",
                ));
            }
            state.status = Some(parse_status(status)?);
            state.headers = headers
                .into_iter()
                .map(|(name, value)| (name.into_bytes(), value.into_bytes()))
                .collect();
        }
        slf.getattr(intern!(slf.py(), "write"))
    }

    /// The `write` callable of PEP 3333: send one chunk immediately.
    fn write(&self, py: Python<'_>, data: &Bound<'_, PyBytes>) -> PyResult<()> {
        self.send_body(py, bytes_from_py(data))
    }
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
fn bytes_from_py(chunk: &Bound<'_, PyBytes>) -> Bytes {
    let data = chunk.as_bytes();
    if data.len() <= COPY_LIMIT {
        Bytes::copy_from_slice(data)
    } else {
        Bytes::from_owner(PyBytesOwner(chunk.clone().unbind()))
    }
}

fn parse_status(status: &str) -> PyResult<u32> {
    let code = status
        .get(..3)
        .and_then(|digits| digits.parse::<u32>().ok())
        .filter(|code| (100..600).contains(code));
    match code {
        Some(code) if status.len() == 3 || status.as_bytes()[3] == b' ' => Ok(code),
        _ => Err(PyValueError::new_err(format!(
            "invalid WSGI status {status:?}: expected three digits, a space and a reason"
        ))),
    }
}

/// Responses that must not be buffered flush immediately (section 5.4).
fn flush_policy(headers: &[(Vec<u8>, Vec<u8>)]) -> pb::FlushPolicy {
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
}

impl InputState {
    /// Make sure some data is pending unless the body has ended.
    fn fill(&mut self, py: Python<'_>) -> bool {
        if !self.pending.is_empty() {
            return true;
        }
        if self.eof {
            return false;
        }
        let next = match self.rx.try_recv() {
            Ok(data) => Some(data),
            Err(TryRecvError::Empty) => {
                let rx = &mut self.rx;
                py.detach(|| rx.blocking_recv())
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
    fn new(rx: mpsc::Receiver<Bytes>) -> Self {
        Self {
            state: Mutex::new(InputState {
                rx,
                pending: Bytes::new(),
                eof: false,
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
fn percent_decode(raw: &[u8]) -> Vec<u8> {
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
    match &head.server {
        Some(server) => {
            environ.set_item(intern!(py, "SERVER_NAME"), &server.address)?;
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
    environ.set_item(intern!(py, "wsgi.version"), (1, 0))?;
    let scheme = if head.scheme.is_empty() {
        "http"
    } else {
        head.scheme.as_str()
    };
    environ.set_item(intern!(py, "wsgi.url_scheme"), scheme)?;
    environ.set_item(intern!(py, "wsgi.input"), input)?;
    environ.set_item(intern!(py, "wsgi.errors"), shared.errors.bind(py))?;
    environ.set_item(intern!(py, "wsgi.multithread"), shared.threads > 1)?;
    environ.set_item(intern!(py, "wsgi.multiprocess"), true)?;
    environ.set_item(intern!(py, "wsgi.run_once"), false)?;
    environ.set_item(intern!(py, "wsgi.input_terminated"), true)?;
    environ.set_item(intern!(py, "muru.request_id"), &head.request_id)?;
    Ok(environ)
}

/// Run one request through the application on the current, attached thread.
pub fn run(
    py: Python<'_>,
    shared: &Shared,
    head: pb::RequestHead,
    body_rx: mpsc::Receiver<Bytes>,
    tx: mpsc::Sender<Outgoing>,
    cancelled: Arc<AtomicBool>,
) {
    let start_response = match Bound::new(py, StartResponse::new(tx)) {
        Ok(start_response) => start_response,
        Err(error) => {
            error.print(py);
            return;
        }
    };

    if let Err(error) = run_app(py, shared, &head, body_rx, &start_response, &cancelled) {
        let broken_pipe = error.is_instance_of::<PyBrokenPipeError>(py);
        if !broken_pipe {
            error.print(py);
        }
        let start_response = start_response.get();
        if !start_response.head_sent() {
            start_response.send_error_response(py);
        } else if !broken_pipe {
            start_response.abort(py, "the application failed after the response started");
        }
    }
}

fn run_app<'py>(
    py: Python<'py>,
    shared: &Shared,
    head: &pb::RequestHead,
    body_rx: mpsc::Receiver<Bytes>,
    start_response: &Bound<'py, StartResponse>,
    cancelled: &AtomicBool,
) -> PyResult<()> {
    let input = Bound::new(py, Input::new(body_rx))?;
    let environ = build_environ(py, shared, head, input)?;
    let result = shared.app.bind(py).call1((environ, start_response))?;
    let outcome = iterate(py, &result, start_response.get(), cancelled);
    // PEP 3333: close() is called whether or not the iteration completed.
    if let Ok(close) = result.getattr(intern!(py, "close")) {
        let closed = close.call0();
        if outcome.is_ok() {
            closed?;
        }
    }
    outcome
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
