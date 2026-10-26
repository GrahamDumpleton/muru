//! State shared by every request in a worker, and the application thread
//! pool that runs them.
//!
//! The pool is a fixed set of threads created when the worker starts and
//! kept for its whole life (design specification section 6.5). Each thread
//! attaches to the interpreter once, so it has a stable Python thread state
//! and thread-local storage that survives from one request to the next, and
//! detaches while it waits for work. Every job runs inside its own copy of
//! the thread's context variables, so values set during one request are gone
//! when the next request runs on the same thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::asgi::{Asgi, Channel};

/// A request the soft timeout interrupted (section 7.6).
pub struct Interrupted {
    /// The request identifier.
    pub request_id: String,
    /// The method.
    pub method: String,
    /// The path and query.
    pub path: String,
    /// Where its thread was, innermost frame first, when it could be read
    /// (section 11.7).
    pub place: Option<String>,
}

/// The innermost frames of a thread as `file line N in function`,
/// innermost first, read without opening source files.
fn thread_place(py: Python<'_>, thread: u64) -> Option<String> {
    let frame = py
        .import("sys")
        .ok()?
        .call_method0("_current_frames")
        .ok()?
        .get_item(thread)
        .ok()?;
    let traceback = py.import("traceback").ok()?;
    let walk = traceback.call_method1("walk_stack", (frame,)).ok()?;
    let options = PyDict::new(py);
    options.set_item("limit", 3).ok()?;
    options.set_item("lookup_lines", false).ok()?;
    let summary = traceback
        .getattr("StackSummary")
        .ok()?
        .call_method("extract", (walk,), Some(&options))
        .ok()?;
    let mut places = Vec::new();

    for entry in summary.try_iter().ok()? {
        let entry = entry.ok()?;
        let file: String = entry.getattr("filename").ok()?.extract().ok()?;
        let line: Option<u64> = entry.getattr("lineno").ok()?.extract().ok()?;
        let name: String = entry.getattr("name").ok()?.extract().ok()?;
        let file = std::path::Path::new(&file)
            .file_name()
            .map_or(file.clone(), |name| name.to_string_lossy().into_owned());
        places.push(format!("{file} line {} in {name}", line.unwrap_or(0)));
    }

    (!places.is_empty()).then(|| places.join(", called from "))
}

/// One request in flight, for the worker's soft timeout, the host's hard
/// timeout and inspection (sections 7.6 and 11.7).
pub struct RequestInfo {
    /// When the worker took it.
    pub started: Instant,
    /// The edge's request id.
    pub request_id: String,
    /// The method.
    pub method: String,
    /// The path, as received.
    pub path: String,
    /// The Python thread serving it, once one has picked it up.
    pub thread: Option<u64>,
    /// The channel of an ASGI request, whose task the soft timeout cancels.
    pub channel: Option<Py<Channel>>,
    /// Wakes a bridge call the application thread is blocked in.
    pub interrupt: Arc<Interrupt>,
    /// The soft timeout has fired for this request.
    pub timed_out: bool,
    /// Exempt from the timeouts: a streaming response or a WebSocket
    /// (section 7.6), which run as long as they need to.
    pub exempt: bool,
}

/// The soft timeout's way into a bridge call the application thread is
/// waiting in (section 7.6): a wait on the body channel or the response
/// channel races against this, so the thread returns to Python bytecode
/// at once, where the injected exception is raised.
pub struct Interrupt {
    fired: AtomicBool,
    notify: tokio::sync::Notify,
    runtime: tokio::runtime::Handle,
}

impl Interrupt {
    fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            fired: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
            runtime,
        }
    }

    /// Wake every wait, now and later.
    pub fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Whether the soft timeout has fired.
    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// Block the calling thread on `future`, returning None if the
    /// interrupt fires first. Called with the interpreter detached.
    pub fn block_on<F: std::future::Future>(&self, future: F) -> Option<F::Output> {
        self.runtime.block_on(async {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.fired() {
                return None;
            }

            tokio::select! {
                output = future => Some(output),
                () = notified => None,
            }
        })
    }
}

/// A unit of work for an application thread.
pub type Job = Box<dyn for<'py> FnOnce(Python<'py>) + Send + 'static>;

/// The names of the threads the runtime created, by Python thread
/// identifier, for inspection (section 11.7): Python's threading module
/// does not know them.
static THREAD_NAMES: Mutex<Option<HashMap<u64, String>>> = Mutex::new(None);

/// Record the calling thread's name under its Python identifier.
pub fn name_this_thread(name: &str) {
    // SAFETY: PyThread_get_thread_ident reads the calling thread's
    // identifier and needs no interpreter state.

    let ident = unsafe { ffi::PyThread_get_thread_ident() } as u64;
    THREAD_NAMES
        .lock()
        .expect("thread names poisoned")
        .get_or_insert_with(HashMap::new)
        .insert(ident, name.to_owned());
}

/// The runtime's thread names by Python identifier, as a dict.
pub fn thread_names(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let names = PyDict::new(py);

    if let Some(known) = THREAD_NAMES.lock().expect("thread names poisoned").as_ref() {
        for (ident, name) in known {
            names.set_item(ident, name)?;
        }
    }

    Ok(names)
}

/// One mount of a release (section 6.7): its application and, for an
/// ASGI mount, the runner scheduled on the loop for each request.
pub struct Mount {
    /// The application callable.
    pub app: Py<PyAny>,
    /// The runner, when the mount is ASGI.
    pub runner: Option<Py<PyAny>>,
}

impl Mount {
    /// A new reference to the same mount.
    pub fn clone_ref(&self, py: Python<'_>) -> Self {
        Self {
            app: self.app.clone_ref(py),
            runner: self.runner.as_ref().map(|runner| runner.clone_ref(py)),
        }
    }
}

/// The soft timeout (section 7.6): how long a request may run before it
/// is interrupted, and the exception class raised in its thread,
/// `muru.RequestTimeout`.
pub struct SoftTimeout {
    /// How long a request may run.
    pub after: Duration,
    /// The exception class to raise.
    pub error: Py<PyAny>,
}

/// State shared by every request handled by this worker.
pub struct Shared {
    /// The default mount's application callable, at /.
    pub app: Py<PyAny>,
    /// The named mounts (section 6.7), by name.
    pub mounts: HashMap<String, Mount>,
    /// The `wsgi.errors` stream (sys.stderr at start-up).
    pub errors: Py<PyAny>,
    /// Application threads, which is also the slot count announced in HELLO.
    pub threads: usize,
    /// wsgi.multiprocess: False only for single-process development runs (section 6.5).
    pub multiprocess: bool,
    /// The event loop side, when the application is ASGI (section 6.6).
    pub asgi: Option<Asgi>,
    /// The application thread pool.
    pub pool: Pool,
    /// The soft timeout (section 7.6), when one is set.
    pub soft_timeout: Option<SoftTimeout>,
    in_flight: AtomicU32,
    total: AtomicU64,
    next_id: AtomicU64,
    /// The requests in flight, by id, for the timeouts (section 7.6)
    /// and for inspection (section 11.7).
    active: Mutex<HashMap<u64, RequestInfo>>,
}

impl Shared {
    /// Create the shared state for one worker, starting its thread pool.
    pub fn new(
        app: Py<PyAny>,
        mounts: HashMap<String, Mount>,
        errors: Py<PyAny>,
        threads: usize,
        multiprocess: bool,
        asgi: Option<Asgi>,
        soft_timeout: Option<SoftTimeout>,
    ) -> std::io::Result<Self> {
        let threads = threads.max(1);
        Ok(Self {
            app,
            mounts,
            errors,
            threads,
            multiprocess,
            asgi,
            pool: Pool::start(threads)?,
            soft_timeout,
            in_flight: AtomicU32::new(0),
            total: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            active: Mutex::new(HashMap::new()),
        })
    }

    /// The application a request head names: the default mount for an
    /// empty name, else the mount of that name.
    pub fn mount(&self, name: &str) -> Option<Mount> {
        if name.is_empty() {
            return Some(Mount {
                app: Python::attach(|py| self.app.clone_ref(py)),
                runner: self.asgi.as_ref().and_then(|asgi| {
                    Python::attach(|py| asgi.runner.as_ref().map(|r| r.clone_ref(py)))
                }),
            });
        }

        self.mounts
            .get(name)
            .map(|mount| Python::attach(|py| mount.clone_ref(py)))
    }

    /// Count a request as started, returning its id for when it finishes
    /// and the interrupt its bridge waits race against. Called on the
    /// link runtime.
    pub fn request_started(
        &self,
        head: &muru_proto::worker::v1::RequestHead,
    ) -> (u64, Arc<Interrupt>) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let interrupt = Arc::new(Interrupt::new(tokio::runtime::Handle::current()));
        self.active
            .lock()
            .expect("active requests poisoned")
            .insert(
                id,
                RequestInfo {
                    started: Instant::now(),
                    request_id: head.request_id.clone(),
                    method: head.method.clone(),
                    path: if head.query.is_empty() {
                        String::from_utf8_lossy(&head.raw_path).into_owned()
                    } else {
                        format!(
                            "{}?{}",
                            String::from_utf8_lossy(&head.raw_path),
                            String::from_utf8_lossy(&head.query)
                        )
                    },
                    thread: None,
                    channel: None,
                    interrupt: Arc::clone(&interrupt),
                    timed_out: false,
                    exempt: false,
                },
            );
        (id, interrupt)
    }

    /// Note the channel of an ASGI request, for the soft timeout.
    pub fn request_channel(&self, id: u64, channel: Py<Channel>) {
        if let Some(info) = self
            .active
            .lock()
            .expect("active requests poisoned")
            .get_mut(&id)
        {
            info.channel = Some(channel);
        }
    }

    /// Exempt a request from the timeouts: its response streams, or it
    /// is a WebSocket (section 7.6).
    pub fn request_exempt(&self, id: u64) {
        if let Some(info) = self
            .active
            .lock()
            .expect("active requests poisoned")
            .get_mut(&id)
        {
            info.exempt = true;
        }
    }

    /// Interrupt every request past the soft timeout that has not been
    /// interrupted yet (section 7.6): a WSGI request gets the timeout
    /// exception injected into its thread and its bridge waits woken, an
    /// ASGI request has its task cancelled. Returns what was interrupted,
    /// for the log and the event.
    pub fn interrupt_overdue(&self, py: Python<'_>) -> Vec<Interrupted> {
        let Some(timeout) = self.soft_timeout.as_ref() else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut interrupted = Vec::new();
        let mut channels = Vec::new();

        // Section 11.7: where each overdue thread is, taken first, before
        // the exception unwinds it, and without the lock, since reading a
        // stack runs Python.

        let overdue: Vec<(u64, u64)> = self
            .active
            .lock()
            .expect("active requests poisoned")
            .iter()
            .filter(|(_, info)| {
                !info.timed_out
                    && !info.exempt
                    && now.saturating_duration_since(info.started) >= timeout.after
            })
            .filter_map(|(id, info)| info.thread.map(|thread| (*id, thread)))
            .collect();
        let places: HashMap<u64, String> = overdue
            .into_iter()
            .filter_map(|(id, thread)| thread_place(py, thread).map(|place| (id, place)))
            .collect();

        // The injection happens under the lock, so a request that ends
        // meanwhile clears it under the same lock (see request_finished).
        // PyThreadState_SetAsyncExc never releases the interpreter, so
        // holding the lock while attached cannot deadlock.

        let mut active = self.active.lock().expect("active requests poisoned");

        for (id, info) in active.iter_mut() {
            if info.timed_out
                || info.exempt
                || now.saturating_duration_since(info.started) < timeout.after
            {
                continue;
            }

            info.timed_out = true;
            interrupted.push(Interrupted {
                request_id: info.request_id.clone(),
                method: info.method.clone(),
                path: info.path.clone(),
                place: places.get(id).cloned(),
            });

            if let Some(channel) = &info.channel {
                channels.push(channel.clone_ref(py));
                continue;
            }

            if let Some(thread) = info.thread {
                // SAFETY: called with the interpreter attached; the thread
                // identifier came from PyThread_get_thread_ident on the
                // serving thread, and the exception is a class the host
                // handed over, kept alive by `timeout_error`.

                unsafe {
                    ffi::PyThreadState_SetAsyncExc(
                        thread as std::os::raw::c_long,
                        timeout.error.as_ptr(),
                    );
                }
            }

            info.interrupt.fire();
        }

        drop(active);

        // Cancelling a task calls into the loop, which may let go of the
        // interpreter, so it happens with the lock released.

        for channel in channels {
            channel.get().cancel(py);
        }

        interrupted
    }

    /// Note which Python thread serves a request: the calling one.
    pub fn request_on_this_thread(&self, id: u64) {
        // SAFETY: PyThread_get_thread_ident reads the calling thread's
        // identifier and needs no interpreter state.

        let thread = unsafe { ffi::PyThread_get_thread_ident() };

        if let Some(info) = self
            .active
            .lock()
            .expect("active requests poisoned")
            .get_mut(&id)
        {
            info.thread = Some(thread as u64);
        }
    }

    /// The requests in flight as Python dicts, for inspection: the
    /// request id, method, path, elapsed milliseconds and the thread
    /// serving it.
    pub fn active_requests<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        let now = Instant::now();

        for info in self
            .active
            .lock()
            .expect("active requests poisoned")
            .values()
        {
            let item = PyDict::new(py);
            item.set_item("request_id", &info.request_id)?;
            item.set_item("method", &info.method)?;
            item.set_item("path", &info.path)?;
            item.set_item(
                "elapsed_ms",
                now.saturating_duration_since(info.started).as_millis() as u64,
            )?;
            item.set_item("thread", info.thread)?;
            list.append(item)?;
        }

        Ok(list)
    }

    /// Count a request as finished. For a WSGI request this runs on its
    /// thread, attached, so a timeout exception injected but not yet
    /// raised is withdrawn here, before the thread serves anything else.
    pub fn request_finished(&self, id: u64) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        let mut active = self.active.lock().expect("active requests poisoned");
        let info = active.remove(&id);

        if let Some(info) = info
            && info.timed_out
            && info.channel.is_none()
            && let Some(thread) = info.thread
        {
            // SAFETY: PyThread_get_thread_ident reads the calling thread's
            // identifier; the calling thread is the one that served the
            // request and is attached to the interpreter, as the WSGI
            // bridge guarantees, and a null exception withdraws a pending
            // one.

            unsafe {
                if ffi::PyThread_get_thread_ident() as u64 == thread {
                    ffi::PyThreadState_SetAsyncExc(
                        thread as std::os::raw::c_long,
                        std::ptr::null_mut(),
                    );
                }
            }
        }
    }

    /// Requests started and not yet finished.
    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Requests started since the worker began.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// How long the longest-running request in flight has been running,
    /// in milliseconds, for the host's hard timeout; zero with none in
    /// flight. Streaming responses and WebSockets do not count.
    pub fn oldest_ms(&self) -> u64 {
        let now = Instant::now();
        self.active
            .lock()
            .expect("active requests poisoned")
            .values()
            .filter(|info| !info.exempt)
            .map(|info| now.saturating_duration_since(info.started).as_millis() as u64)
            .max()
            .unwrap_or(0)
    }
}

/// A fixed pool of long-lived application threads.
pub struct Pool {
    sender: Mutex<Option<mpsc::Sender<Job>>>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl Pool {
    /// Start `threads` application threads.
    pub fn start(threads: usize) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));

        let mut handles = Vec::with_capacity(threads);

        for index in 0..threads {
            let receiver = Arc::clone(&receiver);
            let handle = std::thread::Builder::new()
                .name(format!("muru-app-{index}"))
                .spawn(move || thread_main(&receiver))?;
            handles.push(handle);
        }

        Ok(Self {
            sender: Mutex::new(Some(sender)),
            handles: Mutex::new(handles),
        })
    }

    /// Queue a job for the next idle thread. Returns false once the pool is
    /// shutting down.
    pub fn spawn(&self, job: Job) -> bool {
        let sender = self.sender.lock().expect("pool sender poisoned");

        match sender.as_ref() {
            Some(sender) => sender.send(job).is_ok(),
            None => false,
        }
    }

    /// Stop accepting jobs, let the queued ones finish, and join the threads.
    /// Must be called with the interpreter detached, since the threads need
    /// it to finish their work.
    pub fn shutdown(&self) {
        self.sender.lock().expect("pool sender poisoned").take();
        let handles = std::mem::take(&mut *self.handles.lock().expect("pool handles poisoned"));

        for handle in handles {
            let _ = handle.join();
        }
    }
}

/// One application thread: attach once, then run jobs until the pool closes.
fn thread_main(receiver: &Mutex<mpsc::Receiver<Job>>) {
    if let Some(name) = std::thread::current().name() {
        name_this_thread(name);
    }

    Python::attach(|py| {
        loop {
            // Wait for work with the interpreter detached. Only one idle
            // thread holds the receiver at a time; the others wait for it.

            let next = py.detach(|| receiver.lock().expect("pool receiver poisoned").recv());

            match next {
                Ok(job) => run_in_context(py, job),
                Err(_) => break,
            }
        }
    });
}

/// Run a job inside a fresh copy of the thread's context variables.
fn run_in_context(py: Python<'_>, job: Job) {
    // SAFETY: the thread is attached to the interpreter for the whole
    // function. PyContext_CopyCurrent returns a new reference or null with an
    // exception set; a successful PyContext_Enter is always paired with
    // PyContext_Exit on the same thread before the reference is released.

    unsafe {
        let context = ffi::PyContext_CopyCurrent();

        if context.is_null() {
            report_error(py, "copying the context");
            job(py);
            return;
        }

        if ffi::PyContext_Enter(context) != 0 {
            report_error(py, "entering the context");
            ffi::Py_DECREF(context);
            job(py);
            return;
        }

        job(py);

        if ffi::PyContext_Exit(context) != 0 {
            report_error(py, "leaving the context");
        }

        ffi::Py_DECREF(context);
    }
}

fn report_error(py: Python<'_>, step: &str) {
    if let Some(error) = PyErr::take(py) {
        tracing::error!(%error, "contextvars failure while {step}");
    }
}
