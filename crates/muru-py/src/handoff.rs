//! Handing a WSGI request to an ASGI application (design specification
//! section 6.14, ADR 0042).
//!
//! Every WSGI environ carries `muru.wsgi_to_asgi`. Its `handoff(app)`
//! raises `muru.HandoffNotEnabled` unless the request's mount has
//! `handoff = "asgi"`; when it does, the request continues on the event
//! loop as `app`, with the same link connection. The ASGI channel writes
//! to the request's existing output stream, after a marker that tells the
//! link task, which then feeds the channel the rest of the body, or the
//! WebSocket's messages, and ends the request when the channel does.

use std::os::fd::{OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use muru_proto::worker::v1 as pb;
use pyo3::exceptions::{PyRuntimeError, PyStopIteration};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::asgi::{self, Channel};
use crate::pool::{Interrupt, Pool, Shared};
use crate::wsgi::{Input, Outgoing, Sink, StartResponse};

/// One WSGI request's link to its handoff: the channel, once the request
/// has been handed off, and the uploads it keeps open until the request
/// ends (section 4.11).
#[derive(Default)]
pub struct Slot {
    channel: OnceLock<Py<Channel>>,
    uploads: Mutex<Vec<OwnedFd>>,
}

impl Slot {
    /// The channel the request was handed to, if it was.
    pub fn channel(&self) -> Option<&Channel> {
        self.channel.get().map(Py::get)
    }

    /// Keep the request's uploads open for the application it was
    /// handed to.
    pub fn keep_uploads(&self, uploads: Vec<OwnedFd>) {
        *self.uploads.lock().expect("uploads poisoned") = uploads;
    }
}

/// What the WSGI bridge needs to offer a handoff for one request.
pub struct Handoff {
    /// The request's mount has `handoff = "asgi"`.
    pub enabled: bool,
    /// Where the link task learns of the handoff.
    pub slot: Arc<Slot>,
    /// The link runtime, which feeds the channel the request body.
    pub runtime: tokio::runtime::Handle,
    /// Set by the link when the edge gives up on the request.
    pub cancelled: Arc<AtomicBool>,
    /// The scope state of a request handed from ASGI, for
    /// `environ["muru.state"]`.
    pub state: Option<Py<PyDict>>,
}

/// `environ["muru.wsgi_to_asgi"]`: continue this request as an ASGI
/// application on the worker's event loop.
#[pyclass(frozen, module = "muru._native")]
pub struct WsgiToAsgi {
    enabled: bool,
    head: Arc<pb::RequestHead>,
    uploads: Vec<RawFd>,
    /// The scope, built when first asked for, so WSGI code can fill it
    /// before the handoff.
    scope: Mutex<Option<Py<PyDict>>>,
    start_response: Py<StartResponse>,
    input: Py<Input>,
    shared: Arc<Shared>,
    request: u64,
    tx: mpsc::Sender<Outgoing>,
    interrupt: Arc<Interrupt>,
    runtime: tokio::runtime::Handle,
    cancelled: Arc<AtomicBool>,
    slot: Arc<Slot>,
}

impl WsgiToAsgi {
    #[allow(clippy::too_many_arguments)] // one call site; each argument is a distinct part of the request
    pub fn new(
        handoff: &Handoff,
        head: Arc<pb::RequestHead>,
        uploads: &[RawFd],
        start_response: Py<StartResponse>,
        input: Py<Input>,
        shared: Arc<Shared>,
        request: u64,
        tx: mpsc::Sender<Outgoing>,
        interrupt: Arc<Interrupt>,
    ) -> Self {
        Self {
            enabled: handoff.enabled,
            head,
            uploads: uploads.to_vec(),
            scope: Mutex::new(None),
            start_response,
            input,
            shared,
            request,
            tx,
            interrupt,
            runtime: handoff.runtime.clone(),
            cancelled: Arc::clone(&handoff.cancelled),
            slot: Arc::clone(&handoff.slot),
        }
    }

    fn scope_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let mut scope = self.scope.lock().expect("scope poisoned");

        if let Some(scope) = scope.as_ref() {
            return Ok(scope.bind(py).clone());
        }

        let built = asgi::build_scope(py, &self.head, &self.uploads)?;
        *scope = Some(built.clone().unbind());
        Ok(built)
    }

    /// Put the marker on the request's output stream, ahead of anything
    /// the ASGI application sends.
    fn send_marker(&self, py: Python<'_>) -> bool {
        match self.tx.try_send(Outgoing::Handoff { to_loop: true }) {
            Ok(()) => true,
            Err(TrySendError::Full(item)) => {
                let tx = &self.tx;
                let interrupt = &self.interrupt;
                py.detach(|| match interrupt.block_on(tx.send(item)) {
                    Some(sent) => sent.is_ok(),
                    None => false,
                })
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }

    /// Feed the channel what the WSGI code left of the request body.
    fn forward_body(&self, channel: Py<Channel>, pending: Bytes, mut rx: mpsc::Receiver<Bytes>) {
        let cancelled = Arc::clone(&self.cancelled);

        self.runtime.spawn(async move {
            let channel = channel.get();

            if !pending.is_empty() && !channel.deliver(pending, true).await {
                return;
            }

            while let Some(data) = rx.recv().await {
                if !channel.deliver(data, true).await {
                    return;
                }
            }

            if cancelled.load(Ordering::Relaxed) {
                channel.disconnect();
            } else {
                channel.deliver(Bytes::new(), false).await;
            }
        });
    }
}

#[pymethods]
impl WsgiToAsgi {
    /// The scope the application will receive, mutable until the handoff.
    #[getter]
    fn scope<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        self.scope_dict(py)
    }

    /// Whether the request is a WebSocket upgrade.
    #[getter]
    fn is_websocket(&self) -> bool {
        self.head.scope == pb::Scope::Websocket as i32
    }

    /// Whether the request has been handed off.
    #[getter]
    fn handed_off(&self) -> bool {
        self.slot.channel.get().is_some()
    }

    /// Continue the request as `app` on the event loop. Returns the
    /// empty response the WSGI callable returns.
    fn handoff(&self, py: Python<'_>, app: Bound<'_, PyAny>) -> PyResult<HandedOff> {
        if !self.enabled {
            return Err(not_enabled(py, &self.head.mount, "ASGI", "asgi"));
        }

        if self.slot.channel.get().is_some() {
            return Err(PyRuntimeError::new_err(
                "the request has already been handed off",
            ));
        }

        let start_response = self.start_response.get();

        if start_response.head_sent() {
            return Err(PyRuntimeError::new_err(
                "the request cannot be handed off once its response has started",
            ));
        }

        let Some(event_loop) = self.shared.asgi.as_ref() else {
            return Err(PyRuntimeError::new_err("the worker has no event loop"));
        };

        let scope = self.scope_dict(py)?;
        let channel = Channel::new(
            py,
            event_loop,
            self.tx.clone(),
            &self.head,
            Arc::clone(&self.shared),
            self.request,
            self.runtime.clone(),
        );
        let channel = Bound::new(py, channel)?;

        if self.slot.channel.set(channel.clone().unbind()).is_err() {
            return Err(PyRuntimeError::new_err(
                "the request has already been handed off",
            ));
        }

        // From here the WSGI side is ignored: its status, headers and
        // output, and its iterable, which is closed without being read.

        start_response.hand_off();
        self.shared
            .request_handed_to_loop(self.request, channel.clone().unbind());

        if !self.send_marker(py) {
            return Err(PyRuntimeError::new_err("the client went away"));
        }

        if !self.is_websocket() {
            let (pending, rx) = self.input.get().take_body();
            self.forward_body(channel.clone().unbind(), pending, rx);
        }

        // The application starts on the loop in a copy of this request's
        // context.

        let context = py
            .import(intern!(py, "contextvars"))?
            .call_method0(intern!(py, "copy_context"))?;
        let start = py
            .import(intern!(py, "muru._asgi"))?
            .getattr(intern!(py, "start_handoff"))?;
        let arguments = PyDict::new(py);
        arguments.set_item(intern!(py, "context"), context)?;
        event_loop.event_loop.bind(py).call_method(
            intern!(py, "call_soon_threadsafe"),
            (start, app, scope, &channel),
            Some(&arguments),
        )?;

        Ok(HandedOff)
    }
}

/// `muru.HandoffNotEnabled`, naming the setting the request's mount needs
/// to hand requests to `interface`.
fn not_enabled(py: Python<'_>, mount: &str, interface: &str, setting: &str) -> PyErr {
    let message = if mount.is_empty() {
        format!(
            "this application does not enable handing requests to {interface}; set handoff = \
             \"{setting}\" in its [tool.muru] table or its release's table"
        )
    } else {
        format!(
            "mount {mount} does not enable handing requests to {interface}; set handoff = \
             \"{setting}\" on the mount"
        )
    };

    match py
        .import(intern!(py, "muru"))
        .and_then(|muru| muru.getattr(intern!(py, "HandoffNotEnabled")))
    {
        Ok(class) => PyErr::from_type(class.cast_into().expect("an exception class"), message),
        Err(error) => error,
    }
}

/// `scope["extensions"]["muru"]["asgi_to_wsgi"]` on an http scope:
/// continue this request as a WSGI callable on the thread pool.
#[pyclass(frozen, module = "muru._native")]
pub struct AsgiToWsgi {
    enabled: bool,
    head: Arc<pb::RequestHead>,
    uploads: Vec<RawFd>,
    /// The scope's state, which the WSGI callable sees as `muru.state`.
    state: Py<PyDict>,
    channel: Py<Channel>,
    shared: Arc<Shared>,
    request: u64,
    interrupt: Arc<Interrupt>,
    /// The mount's own threads for handoffs, else the shared pool's.
    pool: Option<Arc<Pool>>,
    runtime: tokio::runtime::Handle,
}

impl AsgiToWsgi {
    #[allow(clippy::too_many_arguments)] // one call site; each argument is a distinct part of the request
    pub fn new(
        enabled: bool,
        head: Arc<pb::RequestHead>,
        uploads: &[RawFd],
        state: Py<PyDict>,
        channel: Py<Channel>,
        shared: Arc<Shared>,
        request: u64,
        interrupt: Arc<Interrupt>,
        pool: Option<Arc<Pool>>,
    ) -> Self {
        Self {
            enabled,
            head,
            uploads: uploads.to_vec(),
            state,
            channel,
            shared,
            request,
            interrupt,
            pool,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

/// Whether a request's deadline, in milliseconds since the epoch, has
/// passed; a request without one has none to pass.
fn past_deadline(deadline_unix_ms: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    deadline_unix_ms != 0 && now >= deadline_unix_ms
}

#[pymethods]
impl AsgiToWsgi {
    /// Whether the request has been handed off.
    #[getter]
    fn handed_off(&self) -> bool {
        self.channel.get().handed_to_wsgi()
    }

    /// Continue the request as the WSGI callable `app` on an application
    /// thread. Returns None, so `return bridge.handoff(app)` ends the
    /// coroutine, which must not use receive or send again.
    fn handoff(&self, py: Python<'_>, app: Bound<'_, PyAny>) -> PyResult<()> {
        if !self.enabled {
            return Err(not_enabled(py, &self.head.mount, "WSGI", "wsgi"));
        }

        let channel = self.channel.get();

        if channel.handed_to_wsgi() {
            return Err(PyRuntimeError::new_err(
                "the request has already been handed off",
            ));
        }

        if channel.response_started() {
            return Err(PyRuntimeError::new_err(
                "the request cannot be handed off once its response has started",
            ));
        }

        // The callable runs in a copy of the coroutine's context.

        let context = py
            .import(intern!(py, "contextvars"))?
            .call_method0(intern!(py, "copy_context"))?
            .unbind();
        let (body, cancelled) = channel.hand_to_wsgi();
        self.shared.request_handed_to_thread(self.request);

        let app = app.unbind();
        let head = (*self.head).clone();
        let uploads = self.uploads.clone();
        let state = self.state.clone_ref(py);
        let shared = Arc::clone(&self.shared);
        let request = self.request;
        let interrupt = Arc::clone(&self.interrupt);
        let tx = channel.sender();
        let runtime = self.runtime.clone();
        let pool = self.pool.clone();

        let job: crate::pool::Job = Box::new(move |py| {
            let sink = Sink {
                tx: tx.clone(),
                interrupt: Arc::clone(&interrupt),
            };

            // Section 6.14: a request that waited for a thread past its
            // deadline is answered 503 queue_full.

            if past_deadline(head.deadline_unix_ms) {
                sink.send(
                    py,
                    Outgoing::Abort {
                        error: "the request waited past its deadline for an application thread"
                            .to_owned(),
                        reason: "queue_full".to_owned(),
                        detail: String::new(),
                    },
                );
                sink.send(py, Outgoing::Finished);
                return;
            }

            if !sink.send(py, Outgoing::Handoff { to_loop: false }) {
                return;
            }

            shared.request_on_this_thread(request);
            let offer = Handoff {
                enabled: false,
                slot: Arc::new(Slot::default()),
                runtime,
                cancelled,
                state: Some(state),
            };

            // SAFETY: the thread is attached; a successful PyContext_Enter
            // is paired with PyContext_Exit on this thread, and the context
            // outlives both.

            let entered = unsafe { pyo3::ffi::PyContext_Enter(context.as_ptr()) == 0 };

            if !entered {
                PyErr::fetch(py).print(py);
            }

            crate::wsgi::run(
                py, &shared, request, &app, head, &uploads, body, tx, &offer, interrupt,
            );

            // SAFETY: as above; this is the exit paired with the enter.

            if entered && unsafe { pyo3::ffi::PyContext_Exit(context.as_ptr()) } != 0 {
                PyErr::fetch(py).print(py);
            }
        });

        let queued = match &pool {
            Some(own) => own.spawn(job),
            None => self.shared.pool.spawn(job),
        };

        if !queued {
            return Err(PyRuntimeError::new_err("the worker is shutting down"));
        }

        Ok(())
    }
}

/// What `handoff()` returns: an empty response iterable that is also a
/// WSGI callable returning itself, so a framework view can return it.
#[pyclass(frozen, module = "muru._native")]
pub struct HandedOff;

#[pymethods]
impl HandedOff {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&self) -> PyResult<()> {
        Err(PyStopIteration::new_err(()))
    }

    /// Called as a WSGI application, by a framework given it as a view's
    /// response: start an empty response, which the bridge ignores.
    fn __call__<'py>(
        slf: &Bound<'py, Self>,
        _environ: &Bound<'py, PyAny>,
        start_response: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, Self>> {
        start_response.call1(("200 OK", Vec::<(String, String)>::new()))?;
        Ok(slf.clone())
    }

    fn close(&self) {}
}
