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
use crate::pool::{Interrupt, Shared};
use crate::wsgi::{Input, Outgoing, StartResponse};

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

    fn not_enabled(&self, py: Python<'_>) -> PyErr {
        let message = if self.head.mount.is_empty() {
            "this application does not enable handing requests to ASGI; set handoff = \"asgi\" \
             in its [tool.muru] table or its release's table"
                .to_owned()
        } else {
            format!(
                "mount {} does not enable handing requests to ASGI; set handoff = \"asgi\" on \
                 the mount",
                self.head.mount
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

    /// Put the marker on the request's output stream, ahead of anything
    /// the ASGI application sends.
    fn send_marker(&self, py: Python<'_>) -> bool {
        match self.tx.try_send(Outgoing::Handoff) {
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
            return Err(self.not_enabled(py));
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
