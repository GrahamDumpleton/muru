//! The `muru._native` extension module.
//!
//! The worker side of the edge-to-worker link, started by a worker after
//! fork, a fixed pool of application threads, the WSGI bridge and the ASGI
//! bridge (design specification sections 5 and 6). The module is declared
//! free-threading compatible (section 6.11): every object shared between
//! threads is guarded by a lock or is immutable.

mod asgi;
mod link;
mod memory;
mod pool;
mod signals;
mod uploads;
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
/// `mounts` are the named mounts (section 6.7) as (name, application,
/// runner, threads) tuples, the runner None for a WSGI mount and the
/// threads, when the mount has its own, as (reserved, max): a WSGI
/// mount runs on a pool of the reserved threads, borrowing the shared
/// pool's beyond them, and a gRPC mount's are declared for the edge;
/// `event_loop` is needed when any mount is ASGI, `runner` when the
/// default one is.
/// `report` is a descriptor the worker writes a status line to every
/// second, for the host's timeouts and recycling (sections 7.6 and 7.7),
/// with its memory measured every `memory_interval_ms`; `on_reclaim`
/// runs on SIGUSR2, when the host asks for memory to be given back.
/// `control` is the worker's end of its control connection to the host
/// (section 11.7) and `on_control` answers each request line on it,
/// called with the line, the requests in flight and the runtime's thread
/// names by identifier. `soft_timeout_ms` is the soft timeout (section
/// 7.6), after which a request is interrupted by raising `timeout_error`
/// (the `muru.RequestTimeout` class) in its thread, or by cancelling its
/// task; both are needed for the timeout to be on. `grpc_methods` are
/// the gRPC methods the gRPC mounts registered (section 6.13), as (path,
/// mount, client streaming, server streaming) tuples; a gRPC mount's
/// runner is the dispatcher of its calls.
///
/// The arguments are the Python signature, keyword by keyword.
#[allow(clippy::too_many_arguments)]
#[pyfunction]
#[pyo3(signature = (socket_path, app, threads=4, multiprocess=true, event_loop=None, runner=None, concurrency=0, on_drain=None, on_exit=None, on_reopen_logs=None, lifeline=None, mounts=Vec::new(), report=None, on_reclaim=None, memory_interval_ms=5000, control=None, on_control=None, soft_timeout_ms=None, timeout_error=None, grpc_methods=Vec::new(), loop_stall_timeout_ms=None))]
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
    mounts: Vec<(String, Py<PyAny>, Option<Py<PyAny>>, Option<(usize, usize)>)>,
    report: Option<i32>,
    on_reclaim: Option<Py<PyAny>>,
    memory_interval_ms: u64,
    control: Option<i32>,
    on_control: Option<Py<PyAny>>,
    soft_timeout_ms: Option<u64>,
    timeout_error: Option<Py<PyAny>>,
    grpc_methods: Vec<(String, String, bool, bool)>,
    loop_stall_timeout_ms: Option<u64>,
) -> PyResult<()> {
    let errors = py.import("sys")?.getattr("stderr")?.unbind();

    let soft_timeout = match (soft_timeout_ms.filter(|&ms| ms > 0), timeout_error) {
        (Some(ms), Some(error)) => Some(pool::SoftTimeout {
            after: std::time::Duration::from_millis(ms),
            error,
        }),
        (Some(_), None) => {
            return Err(PyTypeError::new_err(
                "a soft_timeout_ms needs the timeout_error to raise",
            ));
        }
        (None, _) => None,
    };

    // A runner needs the loop; the loop alone serves ASGI mounts beside a
    // WSGI default.

    let asgi = match (event_loop, runner) {
        (Some(event_loop), runner) => Some(asgi::Asgi {
            event_loop,
            runner,
            resolver: wrap_pyfunction!(asgi::resolve_future, py)?
                .unbind()
                .into_any(),
            concurrency: concurrency.max(1),
        }),
        (None, Some(_)) => {
            return Err(PyTypeError::new_err("a runner needs an event_loop"));
        }
        (None, None) => None,
    };

    let mut named = std::collections::HashMap::new();

    for (name, app, runner, threads) in mounts {
        if runner.is_some() && asgi.is_none() {
            return Err(PyTypeError::new_err(format!(
                "mount {name:?} is ASGI, so an event_loop is needed"
            )));
        }

        if threads.is_some_and(|(reserved, max)| max == 0 || max < reserved) {
            return Err(PyTypeError::new_err(format!(
                "mount {name:?} has threads that cannot run"
            )));
        }

        let pool = match threads {
            Some((reserved, _)) if reserved > 0 && runner.is_none() => Some(Arc::new(
                pool::Pool::start(&format!("muru-app-{name}"), reserved)
                    .map_err(|error| PyRuntimeError::new_err(error.to_string()))?,
            )),
            _ => None,
        };

        named.insert(
            name,
            pool::Mount {
                app,
                runner,
                threads,
                pool,
            },
        );
    }

    let mut shared = pool::Shared::new(
        app,
        named,
        errors,
        threads,
        multiprocess,
        asgi,
        soft_timeout,
    )
    .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    shared.loop_stall = loop_stall_timeout_ms
        .filter(|&ms| ms > 0 && shared.asgi.is_some())
        .map(std::time::Duration::from_millis);
    shared.grpc_methods = grpc_methods
        .into_iter()
        .map(|(path, mount, client_streaming, server_streaming)| {
            muru_proto::worker::v1::GrpcMethod {
                path,
                mount,
                client_streaming,
                server_streaming,
            }
        })
        .collect();
    let shared = Arc::new(shared);
    *SERVING.lock().expect("serving poisoned") = Arc::downgrade(&shared);
    let lifecycle = link::Lifecycle {
        on_drain,
        on_exit,
        on_reopen_logs,
        lifeline,
        report,
        on_reclaim,
        memory_interval: std::time::Duration::from_millis(memory_interval_ms.max(1000)),
        control,
        on_control,
    };

    py.detach(move || link::serve(&socket_path, shared, lifecycle))
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))
}

/// The worker being served, for the functions the runtime library calls
/// from its threads.
static SERVING: std::sync::Mutex<std::sync::Weak<pool::Shared>> =
    std::sync::Mutex::new(std::sync::Weak::new());

/// Section 7.6: the event loop's heartbeat, which the loop stall
/// watchdog waits for.
#[pyfunction]
fn loop_beat() {
    link::loop_beat();
}

/// Take the request the calling thread serves out of the soft and hard
/// timeouts, for as long as a debugger holds it (section 11.7). Returns
/// whether the thread was serving a request.
#[pyfunction]
fn suspend_timeouts() -> bool {
    // SAFETY: PyThread_get_thread_ident reads the calling thread's
    // identifier and needs no interpreter state.

    let thread = unsafe { pyo3::ffi::PyThread_get_thread_ident() } as u64;

    SERVING
        .lock()
        .expect("serving poisoned")
        .upgrade()
        .is_some_and(|shared| shared.exempt_thread(thread))
}

/// The requests in flight, as the control thread is given them: for
/// `muructl debug profile --requests` and `--path` (section 11.7), which
/// look again at every sample.
#[pyfunction]
fn requests(py: Python<'_>) -> PyResult<Bound<'_, pyo3::types::PyList>> {
    let serving = SERVING.lock().expect("serving poisoned").upgrade();

    match serving {
        Some(shared) => shared.active_requests(py),
        None => Ok(pyo3::types::PyList::empty(py)),
    }
}

/// The worker's memory as section 7.7 measures it: resident,
/// proportional and unique sizes in bytes, or none where the platform
/// gives no figures (section 11.7's `muructl debug memory`).
#[pyfunction]
fn memory_usage(py: Python<'_>) -> PyResult<Option<Bound<'_, pyo3::types::PyDict>>> {
    let Some(memory) = memory::measure() else {
        return Ok(None);
    };

    let sizes = pyo3::types::PyDict::new(py);
    sizes.set_item("rss", memory.rss)?;
    sizes.set_item("pss", memory.pss)?;
    sizes.set_item("uss", memory.uss)?;
    Ok(Some(sizes))
}

/// Module initialisation for `muru._native`.
#[pymodule(gil_used = false)]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(serve, m)?)?;
    m.add_function(wrap_pyfunction!(memory_usage, m)?)?;
    m.add_function(wrap_pyfunction!(suspend_timeouts, m)?)?;
    m.add_function(wrap_pyfunction!(requests, m)?)?;
    m.add_function(wrap_pyfunction!(loop_beat, m)?)?;
    m.add_class::<wsgi::Input>()?;
    m.add_class::<wsgi::StartResponse>()?;
    m.add_class::<wsgi::FileWrapper>()?;
    m.add_class::<asgi::Channel>()?;
    m.add_function(wrap_pyfunction!(asgi::resolve_future, m)?)?;
    Ok(())
}
