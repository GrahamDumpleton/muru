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

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use pyo3::ffi;
use pyo3::prelude::*;

use crate::asgi::Asgi;

/// A unit of work for an application thread.
pub type Job = Box<dyn for<'py> FnOnce(Python<'py>) + Send + 'static>;

/// State shared by every request handled by this worker.
pub struct Shared {
    /// The WSGI application callable.
    pub app: Py<PyAny>,
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
    in_flight: AtomicU32,
    total: AtomicU64,
}

impl Shared {
    /// Create the shared state for one worker, starting its thread pool.
    pub fn new(
        app: Py<PyAny>,
        errors: Py<PyAny>,
        threads: usize,
        multiprocess: bool,
        asgi: Option<Asgi>,
    ) -> std::io::Result<Self> {
        let threads = threads.max(1);
        Ok(Self {
            app,
            errors,
            threads,
            multiprocess,
            asgi,
            pool: Pool::start(threads)?,
            in_flight: AtomicU32::new(0),
            total: AtomicU64::new(0),
        })
    }

    /// Count a request as started.
    pub fn request_started(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a request as finished.
    pub fn request_finished(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Requests started and not yet finished.
    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
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
