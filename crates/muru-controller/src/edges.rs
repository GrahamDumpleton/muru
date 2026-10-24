//! The edge processes the daemon runs (design specification section 2.3
//! and ADR 0002): the daemon binds the listening sockets and hands them to
//! an edge spawned from its own executable, pushes routing snapshots over
//! the edge's control socket, and respawns the edge with the same sockets
//! and the last snapshot if it dies, so connections are never refused.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use hyper_util::rt::TokioIo;
use muru_proto::edge::v1 as pb;
use muru_proto::edge::v1::edge_control_client::EdgeControlClient;
use tokio::process::{Child, Command};
use tonic::transport::{Channel, Endpoint, Uri};

/// A listener the daemon bound for the edge.
pub struct Listener {
    /// The bound, listening socket.
    pub socket: std::net::TcpListener,
    /// Whether the edge terminates TLS on it.
    pub tls: bool,
}

/// How an edge is started.
pub struct EdgeSpec {
    /// The executable to spawn, normally the daemon's own.
    pub executable: PathBuf,
    /// The bound, listening sockets the edge serves; the first is the
    /// main one.
    pub listeners: Vec<Listener>,
    /// Where the edge's control socket goes.
    pub control: PathBuf,
    /// Where the edge hands its listener back to a restarted daemon.
    pub listener_socket: PathBuf,
    /// Where the edge's own output goes.
    pub log: PathBuf,
    /// Where the access log goes, or none.
    pub access_log: Option<PathBuf>,
    /// The limits the edge enforces.
    pub limits: muru_edge::Limits,
}

/// The edge process and what it was last told.
pub struct EdgeProcess {
    spec: EdgeSpec,
    process: Process,
    snapshot: pb::Snapshot,
}

/// An edge this daemon spawned, or one it adopted from a previous daemon.
enum Process {
    Child(Child),
    Adopted(u32),
}

/// The descriptor number the first listener takes in the edge process;
/// the others follow it.
const LISTENER_FD: i32 = 3;

/// How many listeners an edge can be handed.
const MAX_LISTENERS: usize = 16;

impl EdgeProcess {
    /// Spawn the edge and wait until its control socket answers.
    pub async fn spawn(spec: EdgeSpec) -> anyhow::Result<Self> {
        let child = start(&spec, &[])?;
        let mut edge = Self {
            spec,
            process: Process::Child(child),
            snapshot: pb::Snapshot::default(),
        };
        edge.wait_ready().await?;
        Ok(edge)
    }

    /// Take over an edge a previous daemon started: it must answer on its
    /// control socket and hand its listeners back, so this daemon can
    /// respawn it later. `tls` says which of them carry TLS, in order.
    /// Its snapshot is replaced by the caller.
    #[allow(clippy::too_many_arguments)] // one call site, every argument a distinct location
    pub async fn adopt(
        pid: u32,
        executable: PathBuf,
        control: PathBuf,
        listener_socket: PathBuf,
        tls: Vec<bool>,
        log: PathBuf,
        access_log: Option<PathBuf>,
        limits: muru_edge::Limits,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(crate::host::alive(pid), "the edge process {pid} is gone");
        let mut client = connect(&control).await?;
        let status = client
            .get_status(pb::EdgeStatusRequest {})
            .await
            .map_err(|status| anyhow::anyhow!("the edge did not answer: {}", status.message()))?
            .into_inner();
        anyhow::ensure!(
            status.pid == pid,
            "the edge on {} is not process {pid}",
            control.display()
        );

        // The listeners come back over the edge's listener socket, in
        // order, until it closes.

        let socket = std::os::unix::net::UnixStream::connect(&listener_socket)
            .with_context(|| format!("connecting to {}", listener_socket.display()))?;
        let mut listeners = Vec::new();
        while let Ok(fd) = crate::fdpass::recv_fd(&socket) {
            listeners.push(Listener {
                socket: std::net::TcpListener::from(fd),
                tls: tls.get(listeners.len()).copied().unwrap_or(false),
            });
        }
        anyhow::ensure!(!listeners.is_empty(), "the edge handed no listener back");

        Ok(Self {
            spec: EdgeSpec {
                executable,
                listeners,
                control,
                listener_socket,
                log,
                access_log,
                limits,
            },
            process: Process::Adopted(pid),
            snapshot: pb::Snapshot {
                version: status.snapshot_version,
                ..Default::default()
            },
        })
    }

    /// The address the edge's first listener serves.
    pub fn listen(&self) -> std::io::Result<std::net::SocketAddr> {
        self.spec.listeners[0].socket.local_addr()
    }

    /// Every listener's address and whether it carries TLS.
    pub fn listeners(&self) -> Vec<(std::net::SocketAddr, bool)> {
        self.spec
            .listeners
            .iter()
            .filter_map(|listener| Some((listener.socket.local_addr().ok()?, listener.tls)))
            .collect()
    }

    /// The edge's pid.
    pub fn pid(&self) -> Option<u32> {
        match &self.process {
            Process::Child(child) => child.id(),
            Process::Adopted(pid) => Some(*pid),
        }
    }

    /// Where the edge hands its listener back.
    pub fn listener_socket(&self) -> &Path {
        &self.spec.listener_socket
    }

    /// Where the edge's control socket is.
    pub fn control(&self) -> &Path {
        &self.spec.control
    }

    /// Leave the edge running and forget it, for a daemon that detaches.
    pub fn release(self) {
        if let Process::Child(mut child) = self.process {
            let _ = child.try_wait();
            std::mem::forget(child);
        }
    }

    /// Whether the edge has exited.
    fn exited(&mut self) -> anyhow::Result<bool> {
        match &mut self.process {
            Process::Child(child) => Ok(child.try_wait()?.is_some()),
            Process::Adopted(pid) => Ok(!crate::host::alive(*pid)),
        }
    }

    /// Push a snapshot, numbered after the last one.
    pub async fn apply(&mut self, snapshot: pb::Snapshot) -> anyhow::Result<()> {
        self.snapshot = pb::Snapshot {
            version: self.snapshot.version + 1,
            ..snapshot
        };
        let mut client = connect(&self.spec.control).await?;
        client
            .apply(self.snapshot.clone())
            .await
            .map_err(|status| {
                anyhow::anyhow!("the edge refused the snapshot: {}", status.message())
            })?;
        Ok(())
    }

    /// Respawn the edge if it has exited, with the same listener and the
    /// last snapshot. Returns whether it had to.
    pub async fn check(&mut self) -> anyhow::Result<bool> {
        if !self.exited()? {
            return Ok(false);
        }

        // The new edge starts with the default workers of the last
        // snapshot, so a connection that waited in the backlog is served,
        // and takes the whole snapshot again right after.

        self.process = Process::Child(start(&self.spec, &self.snapshot.worker_sockets)?);
        self.wait_ready().await?;
        let snapshot = self.snapshot.clone();
        self.apply(snapshot).await?;
        Ok(true)
    }

    /// Tell the edge to stop and wait for it, killing it after a grace period.
    pub async fn stop(mut self) {
        if let Ok(mut client) = connect(&self.spec.control).await {
            let _ = client.stop(pb::StopRequest {}).await;
        }

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
                while crate::host::alive(pid) && started.elapsed() < grace {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if crate::host::alive(pid) {
                    // SAFETY: kill(2) with a valid pid and signal has no
                    // memory-safety preconditions.

                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&self.spec.control);
        let _ = std::fs::remove_file(&self.spec.listener_socket);
    }

    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(30) {
            if self.exited()? {
                anyhow::bail!(
                    "the edge exited before it was ready; see {}",
                    self.spec.log.display()
                );
            }
            if let Ok(mut client) = connect(&self.spec.control).await
                && client.get_status(pb::EdgeStatusRequest {}).await.is_ok()
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!(
            "the edge did not answer on {} within 30 seconds",
            self.spec.control.display()
        )
    }
}

/// Start the edge process with the listeners as descriptors 3, 4 and so
/// on, the workers it serves from the start, and its output appended to
/// its log.
fn start(spec: &EdgeSpec, worker_sockets: &[String]) -> anyhow::Result<Child> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log)
        .with_context(|| format!("opening {}", spec.log.display()))?;
    let mut command = Command::new(&spec.executable);
    command.arg("edge").arg("run");
    for (index, listener) in spec.listeners.iter().enumerate() {
        let fd = LISTENER_FD + index as i32;
        command.arg("--listener").arg(if listener.tls {
            format!("{fd}:tls")
        } else {
            fd.to_string()
        });
    }
    command
        .arg("--control")
        .arg(&spec.control)
        .arg("--listener-socket")
        .arg(&spec.listener_socket)
        .arg("--max-body-size")
        .arg(spec.limits.max_body_size.to_string())
        .arg("--max-header-bytes")
        .arg(spec.limits.max_header_bytes.to_string())
        .arg("--max-headers")
        .arg(spec.limits.max_headers.to_string());
    for (name, duration) in [
        ("queue", spec.limits.queue_timeout),
        ("deadline", spec.limits.request_deadline),
        ("header_read", spec.limits.header_read_timeout),
        ("body_idle", spec.limits.body_idle_timeout),
        ("response_idle", spec.limits.response_idle_timeout),
    ] {
        command
            .arg("--timeout")
            .arg(format!("{name}={}ms", duration.as_millis()));
    }
    if let Some(path) = &spec.access_log {
        command.arg("--access-log").arg(path);
    }
    for socket in worker_sockets {
        command.arg("--worker-socket").arg(socket);
    }
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);

    // Each listener's descriptor is duplicated onto a fixed number in the
    // child, which also clears close-on-exec for that copy, and the edge
    // gets a session of its own so it outlives a daemon that detaches.
    // The sources are raised above the targets first, so no target
    // overwrites a source still to be copied.

    anyhow::ensure!(
        spec.listeners.len() <= MAX_LISTENERS,
        "at most {MAX_LISTENERS} listeners can be handed to an edge"
    );
    let mut raw = [-1i32; MAX_LISTENERS];
    for (slot, listener) in raw.iter_mut().zip(&spec.listeners) {
        *slot = listener.socket.as_raw_fd();
    }
    let count = spec.listeners.len();

    // SAFETY: dup2, fcntl, close and setsid are async-signal-safe and the
    // closure runs in the child between fork and exec, where it touches
    // nothing else and allocates nothing (the arrays are on the stack).

    unsafe {
        command.pre_exec(move || {
            let mut sources = [-1i32; MAX_LISTENERS];
            for index in 0..count {
                let raised = libc::fcntl(raw[index], libc::F_DUPFD, LISTENER_FD + count as i32);
                if raised < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                sources[index] = raised;
            }
            for (index, source) in sources.iter().enumerate().take(count) {
                if libc::dup2(*source, LISTENER_FD + index as i32) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(*source);
            }
            libc::setsid();
            Ok(())
        });
    }

    command.spawn().context("starting the edge process")
}

/// Connect to an edge's control socket.
async fn connect(control: &Path) -> anyhow::Result<EdgeControlClient<Channel>> {
    let control = control.to_path_buf();
    if !control.exists() {
        anyhow::bail!(
            "the edge control socket {} does not exist",
            control.display()
        );
    }

    let endpoint = Endpoint::try_from("http://muru.invalid")?;
    let channel = endpoint
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let control = control.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(control).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;
    Ok(EdgeControlClient::new(channel))
}
