//! The `muru._native` extension module.
//!
//! The worker side of the edge-to-worker link, started by a worker after
//! fork, a fixed pool of application threads, the WSGI bridge and the ASGI
//! bridge (design specification sections 5 and 6). The module is declared
//! free-threading compatible (section 6.11): every object shared between
//! threads is guarded by a lock or is immutable.

mod asgi;
mod link;
mod pool;
mod wsgi;

use std::sync::Arc;

use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;

/// Return the version of the extension, for `muru.__version__` checks.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Serve the worker link on a Unix socket until SIGTERM or SIGINT.
///
/// Called by a worker process after fork. `app` is the application and
/// `threads` the number of application threads: a fixed pool created here
/// and kept for the life of the worker, each thread attached to the
/// interpreter once. The calling thread blocks, with the interpreter
/// detached, until the server stops.
///
/// For an ASGI application `event_loop` is the worker's asyncio loop,
/// already running on its own thread, `runner` the callable scheduled on
/// it with (scope, channel) for each request, and `concurrency` how many
/// requests the loop takes at once (section 6.6).
///
/// `on_drain` is called when the stop signal arrives, before the
/// application threads finish their requests, `on_exit` once they have, and
/// `on_reopen_logs` whenever SIGUSR1 arrives (section 6.3). `lifeline` is
/// a descriptor whose end of file also stops the server: the read end of a
/// pipe the host holds the write end of, so a worker never outlives it.
///
/// The arguments are the Python signature, keyword by keyword.
#[allow(clippy::too_many_arguments)]
#[pyfunction]
#[pyo3(signature = (socket_path, app, threads=4, multiprocess=true, event_loop=None, runner=None, concurrency=0, on_drain=None, on_exit=None, on_reopen_logs=None, lifeline=None))]
fn serve(
    py: Python<'_>,
    socket_path: String,
    app: Py<PyAny>,
    threads: usize,
    multiprocess: bool,
    event_loop: Option<Py<PyAny>>,
    runner: Option<Py<PyAny>>,
    concurrency: usize,
    on_drain: Option<Py<PyAny>>,
    on_exit: Option<Py<PyAny>>,
    on_reopen_logs: Option<Py<PyAny>>,
    lifeline: Option<i32>,
) -> PyResult<()> {
    let errors = py.import("sys")?.getattr("stderr")?.unbind();

    // The loop and the runner come together, or not at all.

    let asgi = match (event_loop, runner) {
        (Some(event_loop), Some(runner)) => Some(asgi::Asgi {
            event_loop,
            runner,
            resolver: wrap_pyfunction!(asgi::resolve_future, py)?
                .unbind()
                .into_any(),
            concurrency: concurrency.max(1),
        }),
        (None, None) => None,
        _ => {
            return Err(PyTypeError::new_err(
                "event_loop and runner must be given together",
            ));
        }
    };

    let shared = pool::Shared::new(app, errors, threads, multiprocess, asgi)
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let shared = Arc::new(shared);
    let lifecycle = link::Lifecycle {
        on_drain,
        on_exit,
        on_reopen_logs,
        lifeline,
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
    m.add_class::<asgi::Channel>()?;
    m.add_function(wrap_pyfunction!(asgi::resolve_future, m)?)?;
    Ok(())
}
