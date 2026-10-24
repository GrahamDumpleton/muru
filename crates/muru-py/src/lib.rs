//! The `muru._native` extension module.
//!
//! Phase 0 scope: the worker side of the edge-to-worker link, started by a
//! worker after fork, a fixed pool of application threads, and a minimal
//! WSGI bridge (design specification sections 5 and 6). The module is
//! declared free-threading compatible from the start (section 6.11): every
//! object shared between threads is guarded by a lock or is immutable.

mod link;
mod pool;
mod wsgi;

use std::sync::Arc;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// Return the version of the extension, for `muru.__version__` checks.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Serve the worker link on a Unix socket until SIGTERM or SIGINT.
///
/// Called by a worker process after fork. `app` is the WSGI callable and
/// `threads` the number of application threads: a fixed pool created here
/// and kept for the life of the worker, each thread attached to the
/// interpreter once. The calling thread blocks, with the interpreter
/// detached, until the server stops.
///
/// `on_drain` is called when the stop signal arrives, before the
/// application threads finish their requests, `on_exit` once they have, and
/// `on_reopen_logs` whenever SIGUSR1 arrives (section 6.3).
#[pyfunction]
#[pyo3(signature = (socket_path, app, threads=4, on_drain=None, on_exit=None, on_reopen_logs=None))]
fn serve(
    py: Python<'_>,
    socket_path: String,
    app: Py<PyAny>,
    threads: usize,
    on_drain: Option<Py<PyAny>>,
    on_exit: Option<Py<PyAny>>,
    on_reopen_logs: Option<Py<PyAny>>,
) -> PyResult<()> {
    let errors = py.import("sys")?.getattr("stderr")?.unbind();
    let shared = pool::Shared::new(app, errors, threads)
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let shared = Arc::new(shared);
    let lifecycle = link::Lifecycle {
        on_drain,
        on_exit,
        on_reopen_logs,
    };

    py.detach(move || link::serve(&socket_path, shared, lifecycle))
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))
}

/// Module initialisation for `muru._native`.
#[pymodule(gil_used = false)]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(serve, m)?)?;
    m.add_class::<wsgi::Input>()?;
    m.add_class::<wsgi::StartResponse>()?;
    m.add_class::<wsgi::FileWrapper>()?;
    Ok(())
}
