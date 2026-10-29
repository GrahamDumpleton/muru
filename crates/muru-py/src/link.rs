//! The worker side of the edge-to-worker link: one request per connection,
//! connections kept open (design specification section 5).
//!
//! Each connection is served by one task. It reads a REQUEST frame, runs the
//! request on an application thread (WSGI) or as a task on the event loop
//! (ASGI), forwards BODY frames to `wsgi.input` or the application's
//! `receive()` until the request body ends or the edge sends ABORT, and
//! writes the response as RESPONSE, BODY and END frames, coalescing small
//! chunks as section 5.4 describes. A CONTINUE frame goes out when the
//! application first reads a body the client is holding back (section
//! 5.5). When both directions have ended the connection is idle again and
//! the next REQUEST may follow. The edge closing the connection cancels
//! the request.

use std::io;
use std::os::fd::{AsFd as _, AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use muru_proto::frame::{self, flag, kind};
use muru_proto::worker::v1 as pb;
use prost::Message;
use pyo3::prelude::*;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;

use muru_proto::descriptors::FdReader;

use crate::asgi;
use crate::pool::Shared;
use crate::wsgi::{self, Outgoing};

/// Errors from the link server.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The largest WebSocket message the worker declares it takes (section
/// 5.6); the edge closes a socket with 1009 for more.
const MAX_MESSAGE_SIZE: u32 = 1024 * 1024;

/// Flush settings from section 5.4, fixed for the spike.
const COALESCE_MIN_BYTES: usize = 16 * 1024;
const COALESCE_MAX_DELAY: Duration = Duration::from_millis(10);

/// Payloads up to this size are copied into one buffer with their header.
const SMALL_PAYLOAD: usize = 16 * 1024;

type Reader = BufReader<FdReader>;
type Writer = OwnedWriteHalf;

/// Python callables run at points in the worker's life (section 6.3).
pub struct Lifecycle {
    /// Runs when the stop signal arrives, before requests in flight finish.
    pub on_drain: Option<Py<PyAny>>,
    /// Runs after the application threads have stopped.
    pub on_exit: Option<Py<PyAny>>,
    /// Runs on SIGUSR1.
    pub on_reopen_logs: Option<Py<PyAny>>,
    /// A descriptor whose end of file means the host is gone.
    pub lifeline: Option<i32>,
    /// A descriptor the worker writes a status line to every second
    /// (section 7.6): the host reads it for timeouts and recycling.
    pub report: Option<i32>,
    /// Runs on SIGUSR2: the host asks for memory to be given back
    /// (section 7.7).
    pub on_reclaim: Option<Py<PyAny>>,
    /// How often the report carries the worker's memory.
    pub memory_interval: Duration,
    /// The worker's end of the control connection to the host (section
    /// 11.7): a stream socket carrying one JSON request per line.
    pub control: Option<i32>,
    /// Answers a control request: called with the request line and the
    /// requests in flight, returns the response line.
    pub on_control: Option<Py<PyAny>>,
    /// How long open WebSockets keep running once the worker is told to
    /// stop, before they are closed with 1001.
    pub websocket_drain: Duration,
    /// How long the event loop's requests then get to finish, the host's
    /// shutdown timeout, before the worker stops waiting for them.
    pub drain: Duration,
}

/// What a signal asks of the worker.
enum Asked {
    Stop,
    ReopenLogs,
    Reclaim,
}

/// How often the worker reports to the host.
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

impl Lifecycle {
    /// Call a hook with the interpreter attached, reporting any exception.
    fn call(hook: Option<&Py<PyAny>>) {
        if let Some(hook) = hook {
            Python::attach(|py| {
                if let Err(error) = hook.call0(py) {
                    wsgi::report(py, &error);
                }
            });
        }
    }
}

/// Bind the socket and serve link connections until SIGTERM or SIGINT, then
/// let the application threads finish their work.
pub fn serve(socket_path: &str, shared: Arc<Shared>, lifecycle: Lifecycle) -> Result<(), BoxError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("muru-link")
        .enable_all()
        .build()?;

    let accepting = Arc::clone(&shared);
    let reopen_logs = lifecycle.on_reopen_logs.as_ref();
    let reclaim = lifecycle.on_reclaim.as_ref();
    let lifeline = lifecycle.lifeline;
    let report = lifecycle.report;
    let memory_interval = lifecycle.memory_interval;

    // Section 11.7: the control thread answers the host's questions,
    // attaching to the interpreter only to do so.

    if let (Some(fd), Some(handler)) = (lifecycle.control, lifecycle.on_control.as_ref()) {
        let handler = Python::attach(|py| handler.clone_ref(py));
        let control_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("muru-control".to_owned())
            .spawn(move || control_thread(fd, handler, control_shared))?;
    }

    let watchdog_stop = Arc::new(AtomicBool::new(false));
    let stop_watchdog = Arc::clone(&watchdog_stop);

    let served: Result<(), BoxError> = runtime.block_on(async move {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)?;

        // The socket's directory carries the edge's group (section 10.9),
        // and the socket opens to that group whatever the umask left.

        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o660))?;
        let mut signals = Signals::new()?;
        let mut lifeline = Lifeline::new(lifeline)?;

        // The first report says the worker is listening; the rest keep
        // the host informed of what it is doing. On a thread of its own,
        // so the host's view (and its hard timeout, section 7.6) never
        // waits on the link's runtime.

        if let Some(fd) = report {
            let report_shared = Arc::clone(&accepting);
            let stop = Arc::clone(&watchdog_stop);
            std::thread::Builder::new()
                .name("muru-report".to_owned())
                .spawn(move || report_thread(fd, &report_shared, memory_interval, &stop))?;
        }

        // Section 7.6: the watchdog interrupts requests past the soft
        // timeout, on a thread of its own so that waiting to attach never
        // holds up the link.

        if let Some(timeout) = accepting.soft_timeout.as_ref().map(|timeout| timeout.after) {
            let watchdog_shared = Arc::clone(&accepting);
            let stop = Arc::clone(&watchdog_stop);
            std::thread::Builder::new()
                .name("muru-watchdog".to_owned())
                .spawn(move || watchdog_loop(&watchdog_shared, timeout, &stop, report))?;
        }

        // Section 7.6: the loop stall watchdog needs no interpreter, so it
        // notices a loop held by code that never lets go.

        if let Some(timeout) = accepting.loop_stall {
            let stop = Arc::clone(&watchdog_stop);
            std::thread::Builder::new()
                .name("muru-loop-watch".to_owned())
                .spawn(move || loop_watch(timeout, &stop, report))?;
        }

        // Section 7.6: the interpreter's heartbeat. A thread attaches to
        // the interpreter every half second, which a thread holding the
        // GIL in a call that never returns stops, and a watchdog that
        // needs no interpreter notices when it has not for the GIL stall
        // timeout.

        if let Some(timeout) = accepting.gil_stall {
            GIL_BEAT_MS.store(clock_ms().max(1), Ordering::Relaxed);
            let stop = Arc::clone(&watchdog_stop);
            std::thread::Builder::new()
                .name("muru-gil-beat".to_owned())
                .spawn(move || gil_beat(&stop))?;
            let stop = Arc::clone(&watchdog_stop);
            std::thread::Builder::new()
                .name("muru-gil-watch".to_owned())
                .spawn(move || gil_watch(timeout, &stop, report))?;
        }

        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    tokio::spawn(serve_connection(stream, Arc::clone(&accepting)));
                }
                asked = signals.next() => match asked {
                    Asked::Stop => break,
                    Asked::ReopenLogs => Lifecycle::call(reopen_logs),
                    Asked::Reclaim => Lifecycle::call(reclaim),
                },
                () = lifeline.gone() => {
                    eprintln!("muru: the host is gone, stopping");
                    break;
                }
            }
        }

        Ok(())
    });

    // Requests already running finish on their threads, or on the event
    // loop, while the runtime still carries their responses; then
    // everything stops.

    Lifecycle::call(lifecycle.on_drain.as_ref());
    shared.shutdown_pools();

    // The pool joins its threads; the loop's requests are waited for here.
    // Open WebSockets keep running for their grace, then are closed with
    // 1001 (section 5.6), and what is left gets the shutdown timeout.

    if shared.asgi.is_some() {
        let wait = |deadline: Instant| {
            while shared.in_flight() > 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        };

        wait(Instant::now() + lifecycle.websocket_drain);
        shared.close_websockets();
        wait(Instant::now() + lifecycle.drain);
    }

    stop_watchdog.store(true, Ordering::Relaxed);
    Lifecycle::call(lifecycle.on_exit.as_ref());
    drop(runtime);
    served
}

/// How often the watchdog looks for requests past the soft timeout.
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(100);

/// Interrupt requests past the soft timeout until told to stop (section
/// 7.6), logging each one with where it was and telling the host, which
/// makes it an event (section 11.7).
fn watchdog_loop(shared: &Arc<Shared>, timeout: Duration, stop: &AtomicBool, report: Option<i32>) {
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(WATCHDOG_INTERVAL);

        if stop.load(Ordering::Relaxed) {
            break;
        }

        let interrupted = Python::attach(|py| shared.interrupt_overdue(py));

        for request in interrupted {
            let place = request
                .place
                .as_deref()
                .map(|place| format!(" at {place}"))
                .unwrap_or_default();
            let message = format!(
                "request {} {} {} ran past the soft timeout of {}{place}",
                request.request_id,
                request.method,
                request.path,
                wsgi::describe_seconds(timeout)
            );
            eprintln!("muru: {message}, interrupting it");

            if let Some(fd) = report {
                report_event(fd, "request.soft_timeout", "warning", &message);
            }
        }
    }
}

/// When the event loop last beat, in milliseconds since the first look
/// at the clock, or 0 before it first did.
static LOOP_BEAT_MS: AtomicU64 = AtomicU64::new(0);

/// How often the loop stall watchdog looks at the heartbeat.
const LOOP_WATCH_INTERVAL: Duration = Duration::from_millis(250);

fn clock_ms() -> u64 {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    let started = STARTED.get_or_init(Instant::now);
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// How often the event loop beats.
const LOOP_BEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Record that the event loop has run (section 7.6), and how much later
/// than due it ran (section 7.3).
pub fn loop_beat() {
    let now = clock_ms().max(1);
    let last = LOOP_BEAT_MS.swap(now, Ordering::Relaxed);

    if last > 0 {
        let interval = Duration::from_millis(now.saturating_sub(last));
        crate::signals::LOOP_LAG.record(interval.saturating_sub(LOOP_BEAT_INTERVAL));
    }
}

/// Tell the host, once a stall, when the event loop has gone longer than
/// `timeout` without beating, so that it replaces the worker as for the
/// hard timeout (section 7.6).
fn loop_watch(timeout: Duration, stop: &AtomicBool, report: Option<i32>) {
    let mut reported = false;

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(LOOP_WATCH_INTERVAL);
        let beat = LOOP_BEAT_MS.load(Ordering::Relaxed);

        if beat == 0 {
            continue;
        }

        let silent = Duration::from_millis(clock_ms().saturating_sub(beat));

        if silent <= timeout {
            reported = false;
            continue;
        }

        if reported {
            continue;
        }

        reported = true;
        let message = format!(
            "the event loop has not run for {:.1} s, past the loop stall timeout of {}",
            silent.as_secs_f64(),
            wsgi::describe_seconds(timeout)
        );
        eprintln!("muru: {message}");

        if let Some(fd) = report {
            report_event(fd, "worker.loop_stalled", "error", &message);
        }
    }
}

/// When a thread last attached to the interpreter (section 7.6), in
/// milliseconds of `clock_ms`, or 0 before the heartbeat started.
static GIL_BEAT_MS: AtomicU64 = AtomicU64::new(0);

/// How often the heartbeat thread attaches to the interpreter.
const GIL_BEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Attach to the interpreter every half second and record the time
/// (section 7.6). A thread that holds the GIL without returning to the
/// interpreter, in a C call or a deadlock, keeps this thread from
/// attaching, and the record goes stale for the watchdog to see. On a
/// free-threaded build attaching never waits for other threads, so the
/// record stays fresh there.
fn gil_beat(stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(GIL_BEAT_INTERVAL);

        if stop.load(Ordering::Relaxed) {
            break;
        }

        Python::attach(|_py| GIL_BEAT_MS.store(clock_ms().max(1), Ordering::Relaxed));
    }
}

/// Tell the host, once a stall, when no thread has attached to the
/// interpreter for longer than `timeout`, so that it replaces the worker
/// as for the hard timeout (section 7.6).
fn gil_watch(timeout: Duration, stop: &AtomicBool, report: Option<i32>) {
    let mut reported = false;

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(LOOP_WATCH_INTERVAL);
        let beat = GIL_BEAT_MS.load(Ordering::Relaxed);

        if beat == 0 {
            continue;
        }

        let silent = Duration::from_millis(clock_ms().saturating_sub(beat));

        if silent <= timeout {
            reported = false;
            continue;
        }

        if reported {
            continue;
        }

        reported = true;
        let message = format!(
            "no thread has run Python for {:.1} s, past the GIL stall timeout of {}",
            silent.as_secs_f64(),
            wsgi::describe_seconds(timeout)
        );
        eprintln!("muru: {message}");

        if let Some(fd) = report {
            report_event(fd, "worker.gil_stalled", "error", &message);
        }
    }
}

/// Write an event for the host to raise on the report descriptor, as one
/// line beside the status lines.
fn report_event(fd: i32, kind: &str, level: &str, message: &str) {
    let line = format!(
        "{{\"event\":{},\"level\":{},\"message\":{}}}\n",
        serde_json_string(kind),
        serde_json_string(level),
        serde_json_string(message)
    );

    // SAFETY: write from a buffer this frame owns, on the descriptor the
    // host handed over for reports; a line under PIPE_BUF is written
    // whole or not at all, and a failed write loses only this event.

    unsafe {
        libc::write(fd, line.as_ptr().cast(), line.len());
    }
}

/// The read end of the host's pipe: readable only at end of file, when
/// the host and its write end are gone (section 2.4).
struct Lifeline {
    fd: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
}

impl Lifeline {
    fn new(fd: Option<i32>) -> io::Result<Self> {
        let Some(raw) = fd else {
            return Ok(Self { fd: None });
        };

        // SAFETY: the descriptor was inherited from the host for this
        // purpose and nothing else in the worker holds it, so owning and
        // closing it here is sound.

        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };

        // SAFETY: fcntl on a descriptor this process owns, setting a flag.

        unsafe {
            let flags = libc::fcntl(raw, libc::F_GETFL);

            if flags >= 0 {
                libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        // SAFETY: the descriptor is valid and owned by `owned`, which is
        // neither closed nor replaced while the AsyncFd holds it.

        let fd = unsafe {
            tokio::io::unix::AsyncFd::register_with_interest(owned, tokio::io::Interest::READABLE)?
        };

        Ok(Self { fd: Some(fd) })
    }

    /// Resolve when the host is gone; never, without a lifeline.
    async fn gone(&mut self) {
        let Some(fd) = &self.fd else {
            return std::future::pending().await;
        };

        loop {
            let Ok(mut guard) = fd.readable().await else {
                return;
            };
            let mut buffer = [0u8; 8];

            // SAFETY: read into a buffer this stack frame owns, on a
            // descriptor this struct owns.

            let read = unsafe {
                libc::read(
                    std::os::fd::AsRawFd::as_raw_fd(fd.get_ref()),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };

            if read == 0 {
                return;
            }

            if read < 0 && std::io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }

            if read < 0 {
                return;
            }
        }
    }
}

/// Answer control requests from the host, one JSON line each, until the
/// connection closes: each is handed to the Python handler with the
/// requests in flight, and its answer written back as one line.
fn control_thread(fd: i32, handler: Py<PyAny>, shared: Arc<Shared>) {
    use std::io::{BufRead as _, Write as _};

    // SAFETY: the descriptor was inherited from the host for this
    // purpose and nothing else in the worker holds it.

    let socket =
        unsafe { <std::os::unix::net::UnixStream as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    crate::pool::name_this_thread("muru-control");
    let _ = socket.set_nonblocking(false);
    let Ok(mut writer) = socket.try_clone() else {
        return;
    };
    let reader = std::io::BufReader::new(socket);

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };

        if line.trim().is_empty() {
            continue;
        }

        // Section 11.7: the bookkeeping is answered here, without the
        // interpreter, so a worker whose threads hold it still says which
        // request each thread serves while its stacks are read from
        // outside.

        if let Some(id) = bookkeeping_request(&line) {
            let text = bookkeeping_answer(id, &shared);

            if writer.write_all(text.as_bytes()).is_err() || writer.write_all(b"\n").is_err() {
                break;
            }

            continue;
        }

        // Section 7.6: so is giving up the requests of a condemned worker,
        // whose stuck code may hold the interpreter for good.

        if let Some((id, which, error, reason)) = abandon_request(&line) {
            let given_up: Vec<String> = shared
                .abandon(which, &error, &reason)
                .iter()
                .map(|request| serde_json_string(request))
                .collect();
            let text = format!(
                "{{\"id\":{id},\"ok\":true,\"result\":{{\"abandoned\":[{}]}}}}",
                given_up.join(",")
            );

            if writer.write_all(text.as_bytes()).is_err() || writer.write_all(b"\n").is_err() {
                break;
            }

            continue;
        }

        let answer = Python::attach(|py| {
            let active = shared.active_requests(py)?;
            let names = crate::pool::thread_names(py)?;
            let reply = handler.call1(py, (line.as_str(), active, names))?;
            reply.extract::<String>(py)
        });

        let text = match answer {
            Ok(text) => text,
            Err(error) => format!(
                "{{\"ok\":false,\"error\":{}}}",
                serde_json_string(&error.to_string())
            ),
        };

        if writer.write_all(text.as_bytes()).is_err() || writer.write_all(b"\n").is_err() {
            break;
        }
    }
}

/// The id of a request line asking for the bookkeeping, as the host
/// writes it: `{"id": N, "op": "bookkeeping", ...}`.
fn bookkeeping_request(line: &str) -> Option<i64> {
    if !line.contains("\"op\": \"bookkeeping\"") {
        return None;
    }

    control_id(line)
}

/// A request line asking the worker to give up requests, as the host
/// writes it: `{"id": N, "op": "abandon", "args": {"which": "oldest" or
/// "all", "reason": R, "error": E}}`, where the reason is a word and the
/// error a sentence with no quotes or backslashes in it.
fn abandon_request(line: &str) -> Option<(i64, crate::pool::Abandon, String, String)> {
    if !line.contains("\"op\": \"abandon\"") {
        return None;
    }

    let which = if line.contains("\"which\": \"all\"") {
        crate::pool::Abandon::All
    } else {
        crate::pool::Abandon::Oldest
    };
    let error = control_string(line, "error").unwrap_or_default();
    let reason = control_string(line, "reason").unwrap_or_default();
    Some((control_id(line)?, which, error, reason))
}

/// The id of a control request line: the number after `"id": `.
fn control_id(line: &str) -> Option<i64> {
    let rest = line.split_once("\"id\": ")?.1;
    let end = rest
        .find(|c: char| c != '-' && !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// A string argument of a control request line, which the host writes
/// without quotes or backslashes in it.
fn control_string(line: &str, key: &str) -> Option<String> {
    let rest = line.split_once(&format!("\"{key}\": \""))?.1;
    Some(rest.split_once('"')?.0.to_owned())
}

/// The requests in flight, with the thread serving each, and the
/// runtime's thread names, as an answer line.
fn bookkeeping_answer(id: i64, shared: &Shared) -> String {
    let requests: Vec<String> = shared
        .active_summary()
        .into_iter()
        .map(|request| {
            format!(
                "{{\"request_id\":{},\"method\":{},\"path\":{},\"elapsed_ms\":{},\"thread\":{},\"native\":{}}}",
                serde_json_string(&request.request_id),
                serde_json_string(&request.method),
                serde_json_string(&request.path),
                request.elapsed_ms,
                request
                    .thread
                    .map_or_else(|| "null".to_owned(), |thread| thread.to_string()),
                request
                    .native
                    .map_or_else(|| "null".to_owned(), |native| native.to_string())
            )
        })
        .collect();
    let names: Vec<String> = crate::pool::thread_name_list()
        .into_iter()
        .map(|(ident, name)| format!("\"{ident}\":{}", serde_json_string(&name)))
        .collect();

    format!(
        "{{\"id\":{id},\"ok\":true,\"result\":{{\"active\":[{}],\"names\":{{{}}}}}}}",
        requests.join(","),
        names.join(",")
    )
}

/// A string as a JSON literal, escaped by hand so the runtime needs no
/// JSON dependency for one error message.
fn serde_json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');

    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }

    out.push('"');
    out
}

/// Write a status line to the report descriptor every second: requests
/// in flight, the age of the oldest, and requests served, and every
/// `memory_interval` the worker's memory too. A write that cannot
/// complete at once (the host is behind) is skipped.
fn report_thread(fd: i32, shared: &Shared, memory_interval: Duration, stop: &AtomicBool) {
    // SAFETY: fcntl on a descriptor inherited for this purpose, setting
    // a flag; the descriptor stays open for the life of the worker.

    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);

        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    let mut last_measured: Option<Instant> = None;

    loop {
        let mut line = format!(
            "{{\"in_flight\":{},\"oldest_ms\":{},\"requests\":{}",
            shared.in_flight(),
            shared.oldest_ms(),
            shared.total()
        );

        if last_measured.is_none_or(|at| at.elapsed() >= memory_interval) {
            last_measured = Some(Instant::now());

            if let Some(memory) = crate::memory::measure() {
                line.push_str(&format!(
                    ",\"rss\":{},\"pss\":{},\"uss\":{}",
                    memory.rss, memory.pss, memory.uss
                ));
            }

            line.push_str(&crate::signals::report_fields());
        }

        line.push_str("}\n");

        // SAFETY: write from a buffer this frame owns, on a descriptor
        // the host handed over for reports; a short or failed write is
        // ignored, since the next report replaces it.

        unsafe {
            libc::write(fd, line.as_ptr().cast(), line.len());
        }

        std::thread::sleep(REPORT_INTERVAL);

        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
}

/// The signals a worker acts on: SIGTERM and SIGINT stop it, SIGUSR1 asks
/// it to reopen its logs, SIGUSR2 to give memory back.
struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    usr1: tokio::signal::unix::Signal,
    usr2: tokio::signal::unix::Signal,
}

impl Signals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            usr1: signal(SignalKind::user_defined1())?,
            usr2: signal(SignalKind::user_defined2())?,
        })
    }

    /// Wait for a signal and say what it asks.
    async fn next(&mut self) -> Asked {
        tokio::select! {
            _ = self.term.recv() => Asked::Stop,
            _ = self.int.recv() => Asked::Stop,
            _ = self.usr1.recv() => Asked::ReopenLogs,
            _ = self.usr2.recv() => Asked::Reclaim,
        }
    }
}

async fn serve_connection(stream: tokio::net::UnixStream, shared: Arc<Shared>) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::with_capacity(32 * 1024, FdReader::new(read_half));
    let mut writer = write_half;

    match frame::read(&mut reader).await {
        Ok(Some(hello)) if hello.kind == kind::HELLO => {
            let Ok(hello) = pb::Hello::decode(hello.payload) else {
                return;
            };

            if !same_major(&hello.protocol_version) {
                return;
            }
        }
        _ => return,
    }

    // Section 5.1: the edge acts on what HELLO declares. A worker whose
    // default mount is ASGI runs requests on its loop, one whose default
    // is WSGI on its threads; a worker with mounts of both kinds declares
    // both, and the edge sizes it by the default mount's capacity.

    let default_is_asgi = shared
        .asgi
        .as_ref()
        .is_some_and(|asgi| asgi.runner.is_some());
    let hello = pb::Hello {
        protocol_version: pb::PROTOCOL_VERSION.to_owned(),
        worker_type: if default_is_asgi { "asgi" } else { "wsgi" }.to_owned(),
        slots: if default_is_asgi {
            0
        } else {
            u32::try_from(shared.threads).unwrap_or(u32::MAX)
        },
        async_slots: shared
            .asgi
            .as_ref()
            .map(|asgi| u32::try_from(asgi.concurrency).unwrap_or(u32::MAX))
            .unwrap_or_default(),
        websockets: shared.asgi.is_some(),
        files: true,
        max_message_size: MAX_MESSAGE_SIZE,
        grpc_methods: shared.grpc_methods.clone(),
        mount_pools: shared.mount_pools(),
    };

    if frame::write(&mut writer, kind::HELLO, 0, hello.encode_to_vec().into())
        .await
        .is_err()
    {
        return;
    }

    let mut early: Option<frame::Frame> = None;

    loop {
        let request = match early.take() {
            Some(request) => request,
            None => match frame::read(&mut reader).await {
                Ok(Some(request)) if request.kind == kind::REQUEST => request,
                // An ABORT or CLOSE for a request that had already ended
                // when the edge sent it: nothing to do.
                Ok(Some(request)) if request.kind == kind::ABORT || request.kind == kind::CLOSE => {
                    continue;
                }
                _ => return,
            },
        };

        let request_end = request.is_end();
        let Ok(head) = pb::RequestHead::decode(request.payload) else {
            return;
        };

        // Section 5.7: the request's stored uploads arrived with its first
        // bytes, one descriptor for each reference, in order.

        let mut uploads = Vec::with_capacity(head.uploads.len());

        for _ in 0..head.uploads.len() {
            match reader.get_mut().take_fd() {
                Some(fd) => uploads.push(fd),
                None => return,
            }
        }

        match handle_request(
            &mut reader,
            &mut writer,
            head,
            uploads,
            request_end,
            &shared,
        )
        .await
        {
            Ok(next) => early = next,
            Err(_) => return,
        }
    }
}

/// What the request direction brought while the response was in progress.
enum Interrupt {
    /// The response completed; nothing more to watch for.
    Done,
    /// The edge has no more use for the request: the client has gone.
    Abort,
    /// The edge closed the connection.
    Closed,
    /// The next request, when the edge saw the response end before this
    /// side stopped watching.
    Request(frame::Frame),
}

/// After the request body has ended, keep watching the request direction
/// until the response completes, for an ABORT (the client went away) or
/// the edge closing the connection. Waiting on the buffer rather than a
/// frame keeps the wait cancel-safe: a frame is only read once its first
/// bytes are there, and then in full.
async fn watch_request(reader: &mut Reader, done: &tokio::sync::Notify) -> io::Result<Interrupt> {
    use tokio::io::AsyncBufReadExt;

    tokio::select! {
        biased;
        _ = done.notified() => Ok(Interrupt::Done),
        buffered = reader.fill_buf() => {
            if buffered?.is_empty() {
                return Ok(Interrupt::Closed);
            }

            match frame::read(reader).await? {
                None => Ok(Interrupt::Closed),
                Some(next) if next.kind == kind::ABORT => Ok(Interrupt::Abort),
                Some(next) if next.kind == kind::REQUEST => Ok(Interrupt::Request(next)),
                Some(next) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected frame type {} after the request body", next.kind),
                )),
            }
        }
    }
}

fn same_major(version: &str) -> bool {
    version.split('.').next() == pb::PROTOCOL_VERSION.split('.').next()
}

/// Serve one request. `uploads` are its stored files' descriptors, held
/// until the request ends (section 4.11).
async fn handle_request(
    reader: &mut Reader,
    writer: &mut Writer,
    head: pb::RequestHead,
    uploads: Vec<OwnedFd>,
    request_end: bool,
    shared: &Arc<Shared>,
) -> io::Result<Option<frame::Frame>> {
    // Section 6.7: the head names the mount; an ASGI mount's request
    // goes to the loop, a WSGI mount's to the thread pool.

    // ADR 0033: a worker of a group that serves a subset of the mounts
    // says so, and the edge's reason names the fault, rather than
    // dropping the connection as a crash would.

    let Some(mount) = shared.mount(&head.mount) else {
        let end = pb::End {
            error: format!("this worker does not serve the mount {:?}", head.mount),
            reason: "mount_not_loaded".to_owned(),
            ..Default::default()
        };
        frame::write(writer, kind::END, 0, end.encode_to_vec().into()).await?;
        return Ok(None);
    };

    if let (Some(asgi), Some(runner)) = (&shared.asgi, mount.runner) {
        return handle_asgi_request(
            reader,
            writer,
            head,
            uploads,
            request_end,
            shared,
            asgi,
            runner,
        )
        .await;
    }

    // Section 6.7: a mount with threads of its own runs on them, unless
    // the edge gave the request a shared thread the mount borrows.

    let pool: &crate::pool::Pool = match &mount.pool {
        Some(own) if !head.borrowed => own,
        _ => &shared.pool,
    };

    let app = mount.app;
    let cancelled = Arc::new(AtomicBool::new(false));
    let (body_tx, body_rx) = mpsc::channel::<Bytes>(8);
    let (out_tx, out_rx) = mpsc::channel::<Outgoing>(32);
    let done = tokio::sync::Notify::new();
    let (request_id, interrupt) = shared.request_started(&head);
    shared.request_outgoing(request_id, out_tx.clone());
    let worker_shared = Arc::clone(shared);
    let worker_cancelled = Arc::clone(&cancelled);
    let queued = pool.spawn(Box::new(move |py| {
        worker_shared.request_on_this_thread(request_id);
        let fds: Vec<RawFd> = uploads.iter().map(AsRawFd::as_raw_fd).collect();
        wsgi::run(
            py,
            &worker_shared,
            request_id,
            &app,
            head,
            &fds,
            body_rx,
            out_tx,
            worker_cancelled,
            interrupt,
        );
        drop(uploads);
        worker_shared.request_finished(request_id);
    }));

    if !queued {
        shared.request_finished(request_id);
        return Err(io::Error::other("the worker is shutting down"));
    }

    let read_side = async {
        let mut body_tx = Some(body_tx);

        if !request_end {
            loop {
                let Some(next) = frame::read(reader).await? else {
                    cancelled.store(true, Ordering::Relaxed);
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the edge closed the connection during the request body",
                    ));
                };
                let end = next.is_end();

                match next.kind {
                    kind::BODY => {
                        if let Some(tx) = &body_tx
                            && !next.payload.is_empty()
                            && tx.send(next.payload).await.is_err()
                        {
                            // The application stopped reading; discard the rest.

                            body_tx = None;
                        }

                        if end {
                            break;
                        }
                    }
                    kind::END => break,
                    kind::ABORT => {
                        // The edge has no more use for the request. Whatever
                        // the application has not read is gone, and its
                        // iteration stops; the connection stays usable.

                        cancelled.store(true, Ordering::Relaxed);
                        break;
                    }
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("unexpected frame type {other} in a request body"),
                        ));
                    }
                }
            }
        }

        drop(body_tx);

        // Section 5.5: a client that goes away while the response is in
        // progress is an ABORT, which stops the iteration.

        loop {
            match watch_request(reader, &done).await? {
                Interrupt::Done => return Ok(None),
                Interrupt::Request(next) => return Ok(Some(next)),
                Interrupt::Abort => cancelled.store(true, Ordering::Relaxed),
                Interrupt::Closed => {
                    cancelled.store(true, Ordering::Relaxed);
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the edge closed the connection during the response",
                    ));
                }
            }
        }
    };
    let write_side = async {
        let result = write_response(out_rx, writer).await;
        done.notify_one();
        result
    };

    let (read_result, write_result) = tokio::join!(read_side, write_side);
    let early = read_result?;
    write_result?;
    Ok(early)
}

/// Run one request as a task on the event loop: body frames become
/// `http.request` messages for the application's `receive()`, ABORT or the
/// end of the response becomes `http.disconnect`, and the output goes
/// through the same coalescing writer as a WSGI response.
#[allow(clippy::too_many_arguments)] // one call site; each argument is a distinct part of the request
async fn handle_asgi_request(
    reader: &mut Reader,
    writer: &mut Writer,
    head: pb::RequestHead,
    uploads: Vec<OwnedFd>,
    request_end: bool,
    shared: &Arc<Shared>,
    asgi: &asgi::Asgi,
    runner: Arc<Py<PyAny>>,
) -> io::Result<Option<frame::Frame>> {
    let (out_tx, out_rx) = mpsc::channel::<Outgoing>(32);
    let done = tokio::sync::Notify::new();

    // The task starts on the loop now; the channel is how the two sides
    // of this function reach it. The uploads stay open until this
    // function returns, when the request has ended.

    let (request_id, _interrupt) = shared.request_started(&head);
    shared.request_outgoing(request_id, out_tx.clone());
    let fds: Vec<RawFd> = uploads.iter().map(AsRawFd::as_raw_fd).collect();
    let _uploads = uploads;

    let channel = match asgi::start(shared, asgi, &runner, &head, &fds, out_tx, request_id) {
        Ok(channel) => channel,
        Err(error) => {
            shared.request_finished(request_id);
            return Err(io::Error::other(format!(
                "the worker is shutting down: {error}"
            )));
        }
    };

    let channel = channel.get();

    if head.scope == pb::Scope::Websocket as i32 {
        return handle_websocket(reader, writer, out_rx, shared, channel, &done, request_id).await;
    }

    if head.scope == pb::Scope::Grpc as i32 {
        return handle_grpc(
            reader, writer, out_rx, shared, channel, &done, request_id, &head,
        )
        .await;
    }

    let read_side = async {
        if request_end {
            channel.deliver(Bytes::new(), false).await;
        } else {
            loop {
                let Some(next) = frame::read(reader).await? else {
                    channel.disconnect();
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the edge closed the connection during the request body",
                    ));
                };
                let end = next.is_end();

                match next.kind {
                    kind::BODY => {
                        if !next.payload.is_empty() || end {
                            channel.deliver(next.payload, !end).await;
                        }

                        if end {
                            break;
                        }
                    }
                    kind::END => {
                        channel.deliver(Bytes::new(), false).await;
                        break;
                    }
                    kind::ABORT => {
                        channel.disconnect();
                        break;
                    }
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("unexpected frame type {other} in a request body"),
                        ));
                    }
                }
            }
        }

        // Section 5.5: the client going away is http.disconnect.

        loop {
            match watch_request(reader, &done).await? {
                Interrupt::Done => return Ok(None),
                Interrupt::Request(next) => return Ok(Some(next)),
                Interrupt::Abort => channel.disconnect(),
                Interrupt::Closed => {
                    channel.disconnect();
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the edge closed the connection during the response",
                    ));
                }
            }
        }
    };
    let write_side = async {
        let result = write_response(out_rx, writer).await;
        channel.complete();
        done.notify_one();
        result
    };

    // The response is over: a receive() still waiting gets http.disconnect.

    let (read_result, write_result) = tokio::join!(read_side, write_side);
    channel.disconnect();
    shared.request_finished(request_id);

    let early = read_result?;
    write_result?;
    Ok(early)
}

/// Run one WebSocket (section 5.6): MESSAGE frames from the edge become
/// `websocket.receive` messages and its CLOSE `websocket.disconnect`, while
/// the application's accept, messages and close go out through the writer.
/// Until the application accepts, the edge may still see the request end
/// with an ordinary response, which the write side signals as done.
async fn handle_websocket(
    reader: &mut Reader,
    writer: &mut Writer,
    out_rx: mpsc::Receiver<Outgoing>,
    shared: &Arc<Shared>,
    channel: &asgi::Channel,
    done: &tokio::sync::Notify,
    request_id: u64,
) -> io::Result<Option<frame::Frame>> {
    use tokio::io::AsyncBufReadExt;

    let mut draining = shared.draining();
    let mut drained = false;

    let read_side = async {
        channel.deliver_connect().await;

        loop {
            // Before the accept, the response ending (a decline) ends the
            // request; after it, only the edge's CLOSE does. A worker told
            // to stop closes an accepted socket itself and waits for the
            // edge's CLOSE in answer. The drain arm is not gated on the
            // accept: a precondition is read once, as the select is built,
            // which is before the application has accepted a socket that
            // has just connected, and the loop only comes round again on
            // a client message. close_for_drain leaves an unaccepted
            // socket alone itself.

            let ended = tokio::select! {
                biased;
                _ = done.notified(), if !channel.accepted() => None,
                _ = draining.wait_for(|draining| *draining), if !drained => {
                    drained = true;
                    channel.close_for_drain();
                    continue;
                }
                buffered = reader.fill_buf() => Some(buffered.map(|buffered| buffered.is_empty())),
            };

            match ended {
                None => return Ok(None),
                Some(Ok(false)) => {}
                Some(Ok(true)) => {
                    channel.disconnect_with(1006, "the edge closed the connection");
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the edge closed the connection during a WebSocket",
                    ));
                }
                Some(Err(error)) => {
                    channel.disconnect_with(1006, "the edge closed the connection");
                    return Err(error);
                }
            }

            let Some(next) = frame::read(reader).await? else {
                channel.disconnect_with(1006, "the edge closed the connection");
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the edge closed the connection during a WebSocket",
                ));
            };

            match next.kind {
                kind::MESSAGE => {
                    let text = next.flags & flag::TEXT != 0;
                    channel.deliver_message(next.payload, text).await;
                }
                kind::CLOSE => {
                    let close = pb::Close::decode(next.payload).unwrap_or_default();
                    let code = u16::try_from(close.code).unwrap_or(1005);
                    channel.disconnect_with(code, &close.reason);
                    return Ok(None);
                }
                kind::ABORT => {
                    channel.disconnect_with(1006, "the client went away");
                    return Ok(None);
                }
                kind::REQUEST => return Ok(Some(next)),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected frame type {other} on a WebSocket"),
                    ));
                }
            }
        }
    };
    let write_side = async {
        let result = write_response(out_rx, writer).await;
        channel.complete();
        done.notify_one();
        result
    };

    let (read_result, write_result) = tokio::join!(read_side, write_side);
    channel.disconnect_with(1006, "the connection ended");
    shared.request_finished(request_id);

    let early = read_result?;
    write_result?;
    Ok(early)
}

/// Run one gRPC call (section 6.13): MESSAGE frames from the edge become
/// `grpc.message` events, its END `grpc.end`, and ABORT or the end of the
/// call `grpc.disconnect`, while the application's metadata, messages and
/// trailers go out as RESPONSE, MESSAGE and END. A streaming call runs as
/// long as it needs to, outside the soft timeout.
#[allow(clippy::too_many_arguments)] // one call site; each argument is a distinct part of the call
async fn handle_grpc(
    reader: &mut Reader,
    writer: &mut Writer,
    out_rx: mpsc::Receiver<Outgoing>,
    shared: &Arc<Shared>,
    channel: &asgi::Channel,
    done: &tokio::sync::Notify,
    request_id: u64,
    head: &pb::RequestHead,
) -> io::Result<Option<frame::Frame>> {
    let path = String::from_utf8_lossy(&head.raw_path);
    let streaming = shared
        .grpc_methods
        .iter()
        .find(|method| {
            method.path == path
                || (method.path.ends_with('/') && path.starts_with(method.path.as_str()))
        })
        .is_none_or(|method| method.client_streaming || method.server_streaming);

    if streaming {
        shared.request_exempt(request_id);
    }

    let read_side = async {
        loop {
            let Some(next) = frame::read(reader).await? else {
                channel.disconnect();
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the edge closed the connection during a gRPC call",
                ));
            };

            match next.kind {
                kind::MESSAGE => channel.deliver_message(next.payload, false).await,
                kind::END => {
                    channel.deliver_end().await;
                    break;
                }
                kind::ABORT => {
                    channel.disconnect();
                    break;
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected frame type {other} in a gRPC call"),
                    ));
                }
            }
        }

        // Section 5.5: the client going away is grpc.disconnect.

        loop {
            match watch_request(reader, done).await? {
                Interrupt::Done => return Ok(None),
                Interrupt::Request(next) => return Ok(Some(next)),
                Interrupt::Abort => channel.disconnect(),
                Interrupt::Closed => {
                    channel.disconnect();
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the edge closed the connection during a gRPC call",
                    ));
                }
            }
        }
    };
    let write_side = async {
        let result = write_grpc_response(out_rx, writer).await;
        channel.complete();
        done.notify_one();
        result
    };

    let (read_result, write_result) = tokio::join!(read_side, write_side);
    channel.disconnect();
    shared.request_finished(request_id);

    let early = read_result?;
    write_result?;
    Ok(early)
}

/// Turn a gRPC call's output into frames as it comes, each message on its
/// own: the head, the messages, and an END with the trailers or the
/// application's failure.
async fn write_grpc_response(
    mut rx: mpsc::Receiver<Outgoing>,
    writer: &mut Writer,
) -> io::Result<()> {
    loop {
        match rx.recv().await {
            Some(Outgoing::Head { headers, .. }) => {
                let head = pb::ResponseHead {
                    status: 200,
                    headers: headers
                        .into_iter()
                        .map(|(name, value)| pb::Header {
                            name: Bytes::from(name),
                            value: Bytes::from(value),
                        })
                        .collect(),
                    flush: pb::FlushPolicy::Immediate as i32,
                    ..Default::default()
                };
                frame::write(writer, kind::RESPONSE, 0, head.encode_to_vec().into()).await?;
            }
            Some(Outgoing::Message { data, .. }) => {
                frame::write(writer, kind::MESSAGE, 0, data).await?;
            }
            Some(Outgoing::Trailers(trailers)) => {
                let end = pb::End {
                    trailers: trailers
                        .into_iter()
                        .map(|(name, value)| pb::Header {
                            name: Bytes::from(name),
                            value: Bytes::from(value),
                        })
                        .collect(),
                    ..Default::default()
                };
                return frame::write(writer, kind::END, 0, end.encode_to_vec().into()).await;
            }
            Some(Outgoing::Abort {
                error,
                reason,
                detail,
            }) => {
                // The edge turns the failure into the call's status.

                let end = pb::End {
                    error,
                    reason,
                    detail,
                    ..Default::default()
                };
                return frame::write(writer, kind::END, 0, end.encode_to_vec().into()).await;
            }
            Some(Outgoing::Finished) | None => {
                let end = pb::End {
                    error: "the application returned without finishing the call".to_owned(),
                    reason: "app_error".to_owned(),
                    ..Default::default()
                };
                return frame::write(writer, kind::END, 0, end.encode_to_vec().into()).await;
            }
            Some(_) => {}
        }
    }
}

/// Turn the application's output into frames, coalescing small chunks.
async fn write_response(mut rx: mpsc::Receiver<Outgoing>, writer: &mut Writer) -> io::Result<()> {
    let mut pending_head: Option<Bytes> = None;
    let mut head_written = false;
    let mut content_length: Option<u64> = None;
    let mut sent: u64 = 0;
    let mut ended = false;
    let mut buffer = BytesMut::new();
    let mut deadline: Option<Instant> = None;
    let mut immediate = false;
    let mut first_block_sent = false;

    // After a 101 the stream carries WebSocket messages, not a body.

    let mut upgraded = false;

    loop {
        let item = match deadline {
            Some(at) => tokio::select! {
                item = rx.recv() => Some(item),
                _ = tokio::time::sleep_until(at.into()) => None,
            },
            None => Some(rx.recv().await),
        };

        // The bridge says when the request is over; the channel closing
        // without that only means the application thread died.

        let item = match item {
            Some(Some(Outgoing::Finished)) => Some(None),
            other => other,
        };

        match item {
            None => {
                // The delay expired with data waiting.

                let data = buffer.split().freeze();
                sent += data.len() as u64;
                let end = content_length == Some(sent);
                write_body(writer, &mut pending_head, &mut head_written, data, end).await?;
                ended |= end;
                deadline = None;
            }
            Some(None) => {
                // The application finished.

                if ended {
                    return Ok(());
                }

                if upgraded {
                    return write_close(writer, 1000, "", false).await;
                }

                if !buffer.is_empty() {
                    let data = buffer.split().freeze();
                    sent += data.len() as u64;
                    let short = content_length.is_some_and(|length| sent < length);
                    write_body(writer, &mut pending_head, &mut head_written, data, !short).await?;

                    if !short {
                        return Ok(());
                    }
                }

                if let Some(head) = pending_head.take() {
                    if content_length.is_some_and(|length| length > 0) {
                        frame::write(writer, kind::RESPONSE, 0, head).await?;
                        head_written = true;
                    } else {
                        return frame::write(writer, kind::RESPONSE, flag::END, head).await;
                    }
                }

                if !head_written {
                    // Nothing at all arrived: the bridge always sends a head, so
                    // this means the application thread died.

                    return Ok(());
                }

                let mut end = pb::End::default();

                if let Some(length) = content_length.filter(|&length| sent < length) {
                    end.error = format!(
                        "the response ended after {sent} of the {length} bytes its Content-Length declared"
                    );
                    eprintln!("muru: {}", end.error);
                }

                return frame::write(writer, kind::END, 0, end.encode_to_vec().into()).await;
            }
            Some(Some(Outgoing::Head {
                status,
                reason,
                headers,
                flush,
            })) => {
                immediate = flush == pb::FlushPolicy::Immediate;
                content_length = headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(b"content-length"))
                    .and_then(|(_, value)| std::str::from_utf8(value).ok()?.trim().parse().ok());
                let head = pb::ResponseHead {
                    status,
                    reason,
                    headers: headers
                        .into_iter()
                        .map(|(name, value)| pb::Header {
                            name: Bytes::from(name),
                            value: Bytes::from(value),
                        })
                        .collect(),
                    flush: flush as i32,
                };

                if status == 101 {
                    // The WebSocket is accepted: the head goes out now and
                    // messages follow it.

                    frame::write(writer, kind::RESPONSE, 0, head.encode_to_vec().into()).await?;
                    head_written = true;
                    upgraded = true;
                    continue;
                }

                pending_head = Some(head.encode_to_vec().into());

                if content_length == Some(0) {
                    // No body will follow; send the head now and finish.

                    let head = pending_head.take().unwrap_or_default();
                    frame::write(writer, kind::RESPONSE, flag::END, head).await?;
                    head_written = true;
                    ended = true;
                }
            }
            Some(Some(Outgoing::Body(mut data))) => {
                if ended {
                    continue;
                }

                // Nothing beyond the declared Content-Length is sent.

                if let Some(length) = content_length {
                    let remaining = length.saturating_sub(sent + buffer.len() as u64);

                    if (data.len() as u64) > remaining {
                        data.truncate(usize::try_from(remaining).unwrap_or(usize::MAX));
                    }
                }

                if immediate || !first_block_sent {
                    first_block_sent = true;

                    if !buffer.is_empty() {
                        let buffered = buffer.split().freeze();
                        sent += buffered.len() as u64;
                        write_body(
                            writer,
                            &mut pending_head,
                            &mut head_written,
                            buffered,
                            false,
                        )
                        .await?;
                    }

                    sent += data.len() as u64;
                    let end = content_length == Some(sent);
                    write_body(writer, &mut pending_head, &mut head_written, data, end).await?;
                    ended |= end;
                } else {
                    buffer.extend_from_slice(&data);

                    if buffer.len() >= COALESCE_MIN_BYTES {
                        let buffered = buffer.split().freeze();
                        sent += buffered.len() as u64;
                        let end = content_length == Some(sent);
                        write_body(writer, &mut pending_head, &mut head_written, buffered, end)
                            .await?;
                        ended |= end;
                        deadline = None;
                    } else if deadline.is_none() {
                        deadline = Some(Instant::now() + COALESCE_MAX_DELAY);
                    }
                }
            }
            Some(Some(Outgoing::File {
                fd,
                offset,
                length,
                more,
            })) => {
                if ended {
                    continue;
                }

                // Section 5.7: what was buffered goes first, then the head
                // if it is still waiting, then the hint with the descriptor
                // attached. The range is cut to the declared Content-Length.

                if !buffer.is_empty() {
                    let buffered = buffer.split().freeze();
                    sent += buffered.len() as u64;
                    write_body(
                        writer,
                        &mut pending_head,
                        &mut head_written,
                        buffered,
                        false,
                    )
                    .await?;
                }

                if let Some(head) = pending_head.take() {
                    frame::write(writer, kind::RESPONSE, 0, head).await?;
                    head_written = true;
                }

                let length = match content_length {
                    Some(declared) => length.min(declared.saturating_sub(sent)),
                    None => length,
                };

                sent += length;
                let end = !more || content_length == Some(sent);
                let hint = pb::FileHint { offset, length }.encode_to_vec();
                let flags = if end { flag::END } else { 0 };
                let encoded = frame::encode(kind::FILE, flags, &hint);
                muru_proto::descriptors::write_with_fd(writer, &encoded, fd.as_fd()).await?;
                drop(fd);
                ended |= end;
                first_block_sent = true;
                deadline = None;
            }
            Some(Some(Outgoing::Finished)) => unreachable!("mapped to the end of output above"),
            Some(Some(Outgoing::Continue)) => {
                // The application is reading the body. The edge may pass
                // that on as 100 Continue (section 5.5); after the response
                // has ended it no longer matters.

                if !ended {
                    frame::write(writer, kind::CONTINUE, 0, Bytes::new()).await?;
                }
            }
            Some(Some(Outgoing::Message { data, text })) => {
                if upgraded {
                    let flags = if text { flag::TEXT } else { 0 };
                    frame::write(writer, kind::MESSAGE, flags, data).await?;
                }
            }
            Some(Some(Outgoing::Trailers(_))) => {
                // Only a gRPC call sends trailers, through its own writer.
            }
            Some(Some(Outgoing::Close {
                code,
                reason,
                draining,
            })) => {
                if upgraded {
                    return write_close(writer, code, &reason, draining).await;
                }
            }
            Some(Some(Outgoing::Abort {
                error,
                reason,
                detail,
            })) => {
                if ended {
                    return Ok(());
                }

                if upgraded {
                    eprintln!("muru: {error}");
                    return write_close(writer, 1011, "internal error", false).await;
                }

                if !buffer.is_empty() {
                    let buffered = buffer.split().freeze();
                    write_body(
                        writer,
                        &mut pending_head,
                        &mut head_written,
                        buffered,
                        false,
                    )
                    .await?;
                }

                // A head the writer was still holding never reached the
                // edge, so the failure is reported in its place and the
                // edge answers with its error page.

                pending_head.take();
                let end = pb::End {
                    error,
                    reason,
                    detail,
                    ..Default::default()
                }
                .encode_to_vec();
                return frame::write(writer, kind::END, 0, end.into()).await;
            }
        }
    }
}

/// End a WebSocket from the worker's side (section 5.6).
async fn write_close(
    writer: &mut Writer,
    code: u16,
    reason: &str,
    draining: bool,
) -> io::Result<()> {
    let close = pb::Close {
        code: u32::from(code),
        reason: reason.to_owned(),
        draining,
    };
    frame::write(writer, kind::CLOSE, 0, close.encode_to_vec().into()).await
}

/// Write a body chunk, with the response head in the same write when it is
/// still pending, and the END flag when this is the last chunk.
async fn write_body(
    writer: &mut Writer,
    pending_head: &mut Option<Bytes>,
    head_written: &mut bool,
    data: Bytes,
    end: bool,
) -> io::Result<()> {
    let flags = if end { flag::END } else { 0 };

    match pending_head.take() {
        Some(head) => {
            *head_written = true;

            if data.len() <= SMALL_PAYLOAD {
                let mut buffer =
                    BytesMut::with_capacity(2 * frame::HEADER_LEN + head.len() + data.len());
                frame::put_header(&mut buffer, kind::RESPONSE, 0, head.len());
                buffer.put_slice(&head);
                frame::put_header(&mut buffer, kind::BODY, flags, data.len());
                buffer.put_slice(&data);
                writer.write_all(&buffer).await
            } else {
                frame::write(writer, kind::RESPONSE, 0, head).await?;
                frame::write(writer, kind::BODY, flags, data).await
            }
        }
        None => frame::write(writer, kind::BODY, flags, data).await,
    }
}
