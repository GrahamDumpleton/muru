//! The host process of one release in development mode: started with the
//! Python interpreter, watched until its workers listen, and stopped with
//! SIGTERM (design specification sections 6.1 and 13.1).

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::logs::Logs;

/// What a host says over its control connection (section 5.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMessage {
    /// The sockets the edge should route to now: the default group's,
    /// and each named group's. Acknowledged by version once the edge
    /// routes by them.
    Workers {
        /// The announcement's number, which the acknowledgement names.
        version: u64,
        /// The default group's sockets.
        sockets: Vec<PathBuf>,
        /// Each named group's sockets.
        groups: Vec<(String, Vec<PathBuf>)>,
    },
    /// An event about a worker, for the event log.
    Event {
        /// The event type, such as worker.recycled.
        kind: String,
        /// The level: info, warning or error.
        level: String,
        /// What happened.
        message: String,
    },
    /// What every worker reported last, for metrics and the dashboard.
    Status {
        /// One entry per worker.
        workers: Vec<WorkerStatus>,
    },
    /// The workers' answers to a request (section 11.7).
    Response {
        /// The request's id.
        id: u64,
        /// One answer per worker asked.
        workers: Vec<WorkerReply>,
        /// Why no worker was asked, when none was.
        error: String,
    },
}

/// One worker's answer to a request (section 11.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerReply {
    /// The worker's index.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// Whether it answered.
    pub ok: bool,
    /// Why not, when it did not.
    pub error: String,
    /// Its answer as JSON text, when it did.
    pub result_json: String,
}

/// What one worker last reported to its host (sections 7.6 and 7.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerStatus {
    /// The worker's index within the host.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// Its group.
    pub group: String,
    /// Requests served since it started.
    pub requests: u64,
    /// Requests in flight.
    pub in_flight: u64,
    /// The age of its oldest request, in milliseconds.
    pub oldest_ms: u64,
    /// Resident set size in bytes, when measured.
    pub rss: Option<u64>,
    /// Proportional set size in bytes, when measured.
    pub pss: Option<u64>,
    /// Unique set size in bytes, when measured.
    pub uss: Option<u64>,
}

impl HostMessage {
    /// Parse one line of the control connection, or none for a line
    /// this version does not know.
    fn parse(line: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let strings = |key: &str| -> Vec<PathBuf> {
            value[key]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(PathBuf::from))
                        .collect()
                })
                .unwrap_or_default()
        };

        match value["type"].as_str()? {
            "workers" => Some(Self::Workers {
                version: value["version"].as_u64().unwrap_or(0),
                sockets: strings("sockets"),
                groups: value["groups"]
                    .as_array()
                    .map(|groups| {
                        groups
                            .iter()
                            .filter_map(|group| {
                                let name = group["name"].as_str()?.to_owned();
                                let sockets = group["sockets"]
                                    .as_array()?
                                    .iter()
                                    .filter_map(|item| item.as_str().map(PathBuf::from))
                                    .collect();
                                Some((name, sockets))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            "event" => Some(Self::Event {
                kind: value["kind"].as_str().unwrap_or("worker.event").to_owned(),
                level: value["level"].as_str().unwrap_or("info").to_owned(),
                message: value["message"].as_str().unwrap_or_default().to_owned(),
            }),
            "response" => Some(Self::Response {
                id: value["id"].as_u64().unwrap_or(0),
                error: value["error"].as_str().unwrap_or_default().to_owned(),
                workers: value["workers"]
                    .as_array()
                    .map(|workers| {
                        workers
                            .iter()
                            .map(|worker| WorkerReply {
                                index: worker["index"].as_u64().unwrap_or(0) as u32,
                                pid: worker["pid"].as_u64().unwrap_or(0) as u32,
                                ok: worker["ok"].as_bool().unwrap_or(false),
                                error: worker["error"].as_str().unwrap_or_default().to_owned(),
                                result_json: worker
                                    .get("result")
                                    .map(|result| result.to_string())
                                    .unwrap_or_default(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            "status" => Some(Self::Status {
                workers: value["workers"]
                    .as_array()
                    .map(|workers| {
                        workers
                            .iter()
                            .map(|worker| WorkerStatus {
                                index: worker["index"].as_u64().unwrap_or(0) as u32,
                                pid: worker["pid"].as_u64().unwrap_or(0) as u32,
                                group: worker["group"].as_str().unwrap_or("default").to_owned(),
                                requests: worker["requests"].as_u64().unwrap_or(0),
                                in_flight: worker["in_flight"].as_u64().unwrap_or(0),
                                oldest_ms: worker["oldest_ms"].as_u64().unwrap_or(0),
                                rss: worker["rss"].as_u64(),
                                pss: worker["pss"].as_u64(),
                                uss: worker["uss"].as_u64(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            _ => None,
        }
    }
}

/// The supervisor's side of a host's control connection: what the host
/// sends arrives on the receiver, and acknowledgements go back.
pub struct HostControl {
    outgoing: mpsc::UnboundedSender<String>,
}

impl HostControl {
    /// Connect to a host's control socket, trying for a few seconds
    /// while the host binds it, and deliver what it sends to the
    /// receiver until the connection ends.
    pub async fn connect(path: &Path) -> anyhow::Result<(Self, mpsc::Receiver<HostMessage>)> {
        let started = Instant::now();

        let stream = loop {
            match tokio::net::UnixStream::connect(path).await {
                Ok(stream) => break stream,
                Err(error) if started.elapsed() < Duration::from_secs(5) => {
                    tracing::debug!(%error, path = %path.display(), "the host control socket is not up yet");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("connecting to {}", path.display()));
                }
            }
        };

        let (read_half, mut write_half) = stream.into_split();
        let (incoming_tx, incoming_rx) = mpsc::channel(32);
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(message) = HostMessage::parse(&line)
                    && incoming_tx.send(message).await.is_err()
                {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            while let Some(line) = outgoing_rx.recv().await {
                if write_half.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        Ok((
            Self {
                outgoing: outgoing_tx,
            },
            incoming_rx,
        ))
    }

    /// Tell the host the edge routes by the sockets it announced at
    /// this version.
    pub fn acknowledge(&self, version: u64) {
        let _ = self
            .outgoing
            .send(format!("{{\"type\":\"applied\",\"version\":{version}}}\n"));
    }

    /// Ask the host's workers something (section 11.7); the answer comes
    /// back as a Response message carrying the same id.
    pub fn request(&self, id: u64, op: &str, worker: Option<u32>, args: &serde_json::Value) {
        let message = serde_json::json!({
            "type": "request",
            "id": id,
            "op": op,
            "worker": worker,
            "args": args,
        });
        let _ = self.outgoing.send(format!("{message}\n"));
    }
}

/// What every host of a run is started with.
pub struct HostSpec {
    /// The interpreter with the muru runtime library installed.
    pub python: PathBuf,
    /// The application as module:callable.
    pub app: String,
    /// The interface the entrypoint speaks.
    pub interface: muru_config::Interface,
    /// Whether the entrypoint is a factory, and when it is called.
    pub factory: Option<muru_config::FactoryPhase>,
    /// Worker processes to fork.
    pub processes: usize,
    /// Application threads per worker.
    pub threads: usize,
    /// Threads for synchronous code called from an ASGI application.
    pub sync_threads: usize,
    /// Requests an ASGI worker's event loop takes at once.
    pub concurrency: usize,
    /// The preload mode.
    pub preload: muru_config::Preload,
    /// The hooks module, if any.
    pub hooks: Option<String>,
    /// The directory to run the host in.
    pub chdir: Option<PathBuf>,
    /// Loopback TCP instead of Unix sockets for the link, from this port.
    pub tcp_base_port: Option<u16>,
    /// Serve in the host process itself, with no fork (section 13.1).
    pub single: bool,
    /// Keep running when this process goes away, and write output to this
    /// file instead of pipes, so a daemon can re-adopt the host after it
    /// restarts (section 13.3). None relays the output to this process.
    pub detach_to: Option<PathBuf>,
    /// Named worker groups beside the default one (section 7.1).
    pub groups: Vec<HostGroup>,
    /// Named mounts beside the default one at / (section 6.7).
    pub mounts: Vec<HostMount>,
    /// Python logging defaults (section 11.4).
    pub logging: muru_config::PythonLogging,
    /// Requests a worker serves before it is replaced (section 7.7).
    pub max_requests: Option<u64>,
    /// How long a worker lives before it is replaced (section 7.7).
    pub max_lifetime: Option<Duration>,
    /// How long a request may run before its worker is replaced (section
    /// 7.6); off when absent.
    pub hard_timeout: Option<Duration>,
    /// How long a replaced worker gets to finish its other requests.
    pub hard_drain_grace: Duration,
    /// How long a worker told to stop gets before it is killed.
    pub shutdown_timeout: Duration,
    /// The memory policy for the workers (section 7.7).
    pub memory: crate::model::MemoryPolicy,
    /// Environment variables for the host and its workers (section
    /// 10.8), secret values already resolved.
    pub env: Vec<(String, String)>,
}

/// A named mount, as the host loads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMount {
    /// The mount's name.
    pub name: String,
    /// The path prefix.
    pub path: String,
    /// The application as module:callable.
    pub entrypoint: String,
    /// The interface the entrypoint speaks.
    pub interface: muru_config::Interface,
}

/// A named worker group's sizes, as the host forks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGroup {
    /// The group's name.
    pub name: String,
    /// Worker processes.
    pub processes: usize,
    /// Application threads per worker.
    pub threads: usize,
    /// Threads for synchronous code called from an ASGI application.
    pub sync_threads: usize,
    /// Requests an ASGI worker's event loop takes at once.
    pub concurrency: usize,
}

/// A running host and the sockets its workers listen on.
pub struct HostProcess {
    process: Process,
    stdin: Option<ChildStdin>,
    /// Every worker link socket (paths, or addresses for the TCP link):
    /// the default group's first, then each named group's.
    pub sockets: Vec<PathBuf>,
    /// The sockets of each named group, in the order of the spec.
    pub group_sockets: Vec<(String, Vec<PathBuf>)>,
    run_dir: PathBuf,
    /// The release number within this run, from 1.
    pub release: u32,
}

/// A host this process spawned, or one it adopted from a previous daemon.
enum Process {
    Child(Child),
    Adopted(u32),
}

impl HostProcess {
    /// Start the host for release number `release` under the run directory.
    /// Its output is relayed to this process's own streams and kept in
    /// `logs` for the dashboard.
    pub fn start(
        spec: &HostSpec,
        run_dir: &Path,
        release: u32,
        logs: &Logs,
    ) -> anyhow::Result<Self> {
        let run_dir = run_dir.join(format!("r{release}"));
        std::fs::create_dir_all(&run_dir)
            .with_context(|| format!("creating {}", run_dir.display()))?;
        let mut command = Command::new(&spec.python);
        command
            .arg("-m")
            .arg("muru.host")
            .arg("--app")
            .arg(&spec.app)
            .arg("--socket-dir")
            .arg(&run_dir)
            .arg("--processes")
            .arg(spec.processes.to_string())
            .arg("--threads")
            .arg(spec.threads.to_string())
            .arg("--sync-threads")
            .arg(spec.sync_threads.to_string())
            .arg("--concurrency")
            .arg(spec.concurrency.to_string())
            .arg("--preload")
            .arg(spec.preload.as_str())
            .arg("--interface")
            .arg(spec.interface.as_str())
            .stdin(Stdio::piped());

        match &spec.detach_to {
            Some(log) => {
                let log = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)
                    .with_context(|| format!("opening {}", log.display()))?;
                command.stdout(log.try_clone()?).stderr(log);

                // Its own session, so the daemon's signals and terminal do
                // not reach it.
                // SAFETY: setsid is async-signal-safe and the closure runs
                // in the child between fork and exec.

                unsafe {
                    command.pre_exec(|| {
                        libc::setsid();
                        Ok(())
                    });
                }
            }
            None => {
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
            }
        }

        if let Some(hooks) = &spec.hooks {
            command.arg("--hooks").arg(hooks);
        }

        if let Some(phase) = spec.factory {
            command
                .arg("--factory")
                .arg("--factory-phase")
                .arg(phase.as_str());
        }

        if spec.single {
            command.arg("--single");
        }

        if let Some(log) = &spec.detach_to {
            command.arg("--detach").arg("--log-file").arg(log);
        }

        for mount in &spec.mounts {
            command.arg("--mount").arg(format!(
                "{}={}={}={}",
                mount.name,
                mount.path,
                mount.entrypoint,
                mount.interface.as_str()
            ));
        }

        for group in &spec.groups {
            command.arg("--group").arg(format!(
                "{}={},{},{},{}",
                group.name, group.processes, group.threads, group.sync_threads, group.concurrency
            ));
        }

        if let Some(level) = spec.logging.level {
            command.arg("--log-level").arg(level.as_str());
        }

        if let Some(format) = &spec.logging.format {
            command.arg("--log-format").arg(format);
        }

        if spec.logging.capture_warnings == Some(true) {
            command.arg("--capture-warnings");
        }

        for (name, level) in &spec.logging.loggers {
            command
                .arg("--logger")
                .arg(format!("{name}={}", level.as_str()));
        }

        let workers = spec.processes
            + spec
                .groups
                .iter()
                .map(|group| group.processes)
                .sum::<usize>();
        let base_port = spec
            .tcp_base_port
            .map(|base| base.wrapping_add((release - 1) as u16 * workers as u16));

        if let Some(base) = base_port {
            command.arg("--tcp-base-port").arg(base.to_string());
        }

        // Section 7.6 and 7.7: the host enforces these on its workers and
        // tells this process about replacements over its control socket.

        command
            .arg("--control")
            .arg(run_dir.join("control.sock"))
            .arg("--hard-drain-grace")
            .arg(spec.hard_drain_grace.as_secs_f64().to_string())
            .arg("--shutdown-timeout")
            .arg(spec.shutdown_timeout.as_secs_f64().to_string());

        if let Some(count) = spec.max_requests {
            command.arg("--max-requests").arg(count.to_string());
        }

        if let Some(lifetime) = spec.max_lifetime {
            command
                .arg("--max-lifetime")
                .arg(lifetime.as_secs_f64().to_string());
        }

        if let Some(timeout) = spec.hard_timeout {
            command
                .arg("--hard-timeout")
                .arg(timeout.as_secs_f64().to_string());
        }

        // Section 7.7: the memory policy.

        command
            .arg("--memory-measure")
            .arg(spec.memory.measure.as_str())
            .arg("--memory-soft-for")
            .arg((spec.memory.soft_for_ms as f64 / 1000.0).to_string())
            .arg("--memory-reclaim-first")
            .arg(spec.memory.reclaim_first.to_string())
            .arg("--memory-check-interval")
            .arg((spec.memory.check_interval_ms as f64 / 1000.0).to_string());

        if let Some(bytes) = spec.memory.soft_limit {
            command.arg("--memory-soft-limit").arg(bytes.to_string());
        }

        if let Some(bytes) = spec.memory.hard_limit {
            command.arg("--memory-hard-limit").arg(bytes.to_string());
        }

        if let Some(dir) = &spec.chdir {
            command.current_dir(dir);
        }

        command.envs(spec.env.iter().map(|(name, value)| (name, value)));

        let mut child = command
            .spawn()
            .with_context(|| format!("starting the host with {}", spec.python.display()))?;

        // The host exits when this end of its stdin closes, whatever happens here.

        let stdin = child.stdin.take();

        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(relay(stdout, logs.clone(), false));
        }

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(relay(stderr, logs.clone(), true));
        }

        // Sockets are numbered across every group in the order the host
        // forks them: the default group first, then each named group.

        let socket = |group: Option<&str>, index: usize, position: usize| match base_port {
            None => match group {
                None => run_dir.join(format!("worker-{index}.sock")),
                Some(name) => run_dir.join(format!("worker-{name}-{index}.sock")),
            },
            Some(base) => {
                let port = base.wrapping_add(u16::try_from(position).unwrap_or(0));
                PathBuf::from(format!("127.0.0.1:{port}"))
            }
        };
        let mut sockets: Vec<PathBuf> = (0..spec.processes)
            .map(|index| socket(None, index, index))
            .collect();
        let mut group_sockets = Vec::new();

        for group in &spec.groups {
            let mut these = Vec::new();

            for index in 0..group.processes {
                let path = socket(Some(&group.name), index, sockets.len());
                sockets.push(path.clone());
                these.push(path);
            }

            group_sockets.push((group.name.clone(), these));
        }

        Ok(Self {
            process: Process::Child(child),
            stdin,
            sockets,
            group_sockets,
            run_dir,
            release,
        })
    }

    /// Take over a host a previous daemon started: it is checked to be
    /// alive and its workers to be listening, and is stopped with signals
    /// rather than waited for, since it is not this process's child.
    pub fn adopt(
        pid: u32,
        sockets: Vec<PathBuf>,
        group_sockets: Vec<(String, Vec<PathBuf>)>,
        run_dir: PathBuf,
        release: u32,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(alive(pid), "process {pid} is gone");
        anyhow::ensure!(
            sockets.iter().all(|socket| listening(socket)),
            "the workers of process {pid} are not listening"
        );

        Ok(Self {
            process: Process::Adopted(pid),
            stdin: None,
            sockets,
            group_sockets,
            run_dir,
            release,
        })
    }

    /// The default group's sockets: those of no named group.
    pub fn default_sockets(&self) -> Vec<PathBuf> {
        let named: usize = self
            .group_sockets
            .iter()
            .map(|(_, sockets)| sockets.len())
            .sum();

        self.sockets[..self.sockets.len() - named].to_vec()
    }

    /// The host's pid.
    pub fn pid(&self) -> Option<u32> {
        match &self.process {
            Process::Child(child) => child.id(),
            Process::Adopted(pid) => Some(*pid),
        }
    }

    /// Send the host a signal. Returns whether it was delivered.
    pub fn signal(&self, signal: i32) -> bool {
        let Some(pid) = self.pid() else {
            return false;
        };

        // SAFETY: kill with a pid this process started or adopted.

        unsafe { libc::kill(pid as libc::pid_t, signal) == 0 }
    }

    /// The release run directory.
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Where the host serves its control connection (section 5.9): the
    /// worker sockets as they change, and events about its workers.
    pub fn control_path(&self) -> PathBuf {
        self.run_dir.join("control.sock")
    }

    /// Wait until every worker listens, or the host exits or the time is up.
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let started = Instant::now();

        loop {
            if self.sockets.iter().all(|socket| listening(socket)) {
                return Ok(());
            }

            if let Some(status) = self.try_exited()? {
                anyhow::bail!("the host {status} before its workers were ready");
            }

            anyhow::ensure!(
                started.elapsed() < timeout,
                "the workers did not start within {} seconds",
                timeout.as_secs()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Resolve when the host exits.
    pub async fn exited(&mut self) -> std::io::Result<ExitStatus> {
        match &mut self.process {
            Process::Child(child) => child.wait().await,
            Process::Adopted(pid) => {
                let pid = *pid;

                while alive(pid) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }

                Ok(ExitStatus::default())
            }
        }
    }

    /// Whether the host has exited, without waiting: a sentence saying how
    /// when it has.
    pub fn try_exited(&mut self) -> std::io::Result<Option<String>> {
        match &mut self.process {
            Process::Child(child) => Ok(child
                .try_wait()?
                .map(|status| format!("exited with {status}"))),
            Process::Adopted(pid) => Ok((!alive(*pid)).then(|| "is gone".to_owned())),
        }
    }

    /// Stop the host: SIGTERM and its stdin closed, then SIGKILL after a
    /// grace period. Its workers finish the requests they are serving.
    pub async fn stop(mut self) {
        if let Some(pid) = self.pid() {
            // SAFETY: kill(2) with a valid pid and signal has no memory-safety
            // preconditions; the pid is a host this daemon started or adopted.

            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }

        drop(self.stdin.take());

        let grace = Duration::from_secs(10);

        match &mut self.process {
            Process::Child(child) => {
                if tokio::time::timeout(grace, child.wait()).await.is_err() {
                    let _ = child.kill().await;
                }
            }
            Process::Adopted(pid) => {
                let pid = *pid;
                let started = Instant::now();

                while alive(pid) && started.elapsed() < grace {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }

                if alive(pid) {
                    // SAFETY: as above.

                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
            }
        }

        let _ = std::fs::remove_dir_all(&self.run_dir);
    }

    /// Leave the host running and forget it, for a daemon that detaches.
    pub fn release(self) {
        drop(self.stdin);

        if let Process::Child(mut child) = self.process {
            // The child must not be killed when its handle drops.

            let _ = child.try_wait();
            std::mem::forget(child);
        }
    }
}

/// Whether a process exists. A child of this process that has exited is
/// reaped here and reported gone: after an upgrade (section 13.4) the
/// hosts and the edge are children the previous image started, which
/// nothing else waits for.
pub fn alive(pid: u32) -> bool {
    let mut status = 0;

    // SAFETY: waitpid with WNOHANG on one pid returns at once and reaps
    // the process only when it is a child of this one that has exited;
    // kill with signal 0 checks for the process and sends nothing.

    unsafe {
        if libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) == pid as libc::pid_t {
            return false;
        }

        libc::kill(pid as libc::pid_t, 0) == 0
    }
}

/// Whether a worker accepts connections on its socket. The edge connects
/// to every worker as soon as the host is ready, so this connects too: a
/// Unix socket file exists from bind, a moment before listen.
fn listening(socket: &Path) -> bool {
    match socket.to_str() {
        Some(address) if address.starts_with("127.0.0.1:") => {
            std::net::TcpStream::connect(address).is_ok()
        }
        _ => std::os::unix::net::UnixStream::connect(socket).is_ok(),
    }
}

/// Copy a host stream to the matching stream of this process, line by
/// line, keeping each line for the dashboard.
async fn relay<R: AsyncRead + Unpin>(stream: R, logs: Logs, to_stderr: bool) {
    let mut lines = BufReader::new(stream).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if to_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
        logs.push(line);
    }
}
