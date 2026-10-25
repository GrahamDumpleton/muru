//! The edge processes the daemon runs (design specification section 2.3
//! and ADR 0002): the daemon binds the listening sockets and hands them to
//! an edge spawned from its own executable, pushes routing snapshots over
//! the edge's control socket, and respawns the edge with the same sockets
//! and the last snapshot if it dies, so connections are never refused. An
//! upgrade (section 13.4) replaces the edge the same way, with the old one
//! finishing its connections once the new one serves.

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
    /// Numbers the edge's sockets, so those of a replacement never
    /// collide with those of the edge it replaces.
    pub generation: u32,
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

/// The control socket and the listener-return socket of an edge of the
/// given generation, in the runtime directory.
pub fn socket_paths(run_dir: &Path, generation: u32) -> (PathBuf, PathBuf) {
    (
        run_dir.join(format!("edge-{generation}.sock")),
        run_dir.join(format!("edge-{generation}-listener.sock")),
    )
}

impl EdgeProcess {
    /// Spawn the edge and wait until its control socket answers.
    pub async fn spawn(spec: EdgeSpec) -> anyhow::Result<Self> {
        let child = launch(&spec, &[]).await?;
        Ok(Self {
            spec,
            process: Process::Child(child),
            snapshot: pb::Snapshot::default(),
        })
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
        generation: u32,
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
                generation,
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
        pid_of(&self.process)
    }

    /// The generation of the edge's sockets.
    pub fn generation(&self) -> u32 {
        self.spec.generation
    }

    /// How long the edge is given to finish its connections when it is
    /// replaced: the request deadline, so a response in flight can
    /// complete, and a little over.
    pub fn drain_grace(&self) -> Duration {
        self.spec.limits.request_deadline + Duration::from_secs(5)
    }

    /// Move the spec's sockets to the next generation.
    fn next_generation(&mut self) {
        let run_dir = self
            .spec
            .control
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        self.spec.generation += 1;
        let (control, listener_socket) = socket_paths(&run_dir, self.spec.generation);
        self.spec.control = control;
        self.spec.listener_socket = listener_socket;
    }

    /// Send the edge a signal. Returns whether it was delivered.
    pub fn signal(&self, signal: i32) -> bool {
        let Some(pid) = self.pid() else {
            return false;
        };

        // SAFETY: kill with a pid this process started or adopted.

        unsafe { libc::kill(pid as libc::pid_t, signal) == 0 }
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

    /// The version of the snapshot the edge was last given.
    pub fn snapshot_version(&self) -> u64 {
        self.snapshot.version
    }

    /// Recent requests, the last minute's counters and the pools' state,
    /// for the dashboard (section 10.7).
    pub async fn recent(&self, limit: u32) -> anyhow::Result<pb::RecentRequests> {
        let mut client = connect(&self.spec.control).await?;
        Ok(client
            .get_recent(pb::RecentRequest { limit })
            .await
            .map_err(|status| anyhow::anyhow!("the edge did not answer: {}", status.message()))?
            .into_inner())
    }

    /// The edge's metrics as samples (section 11.1).
    pub async fn metrics(&self) -> anyhow::Result<Vec<muru_edge::metrics::Sample>> {
        let mut client = connect(&self.spec.control).await?;
        let report = client
            .get_metrics(pb::MetricsRequest {})
            .await
            .map_err(|status| anyhow::anyhow!("the edge did not answer: {}", status.message()))?
            .into_inner();
        Ok(report
            .samples
            .into_iter()
            .map(muru_edge::metrics::Sample::from_message)
            .collect())
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

        self.next_generation();
        self.process = Process::Child(launch(&self.spec, &self.snapshot.worker_sockets).await?);
        let snapshot = self.snapshot.clone();
        self.apply(snapshot).await?;
        Ok(true)
    }

    /// Section 13.4: start a new edge on the same listeners, give it the
    /// current snapshot, and only then tell the old one to stop: it stops
    /// accepting, finishes its connections and exits, waited for in the
    /// background for `grace` before it is killed. Returns the old edge's
    /// pid. If the new edge does not come up or refuses the snapshot, it
    /// is discarded and the old one keeps serving.
    pub async fn replace(&mut self, grace: Duration) -> anyhow::Result<u32> {
        let old_control = self.spec.control.clone();
        let old_listener_socket = self.spec.listener_socket.clone();
        let old_generation = self.spec.generation;
        self.next_generation();

        let next = match launch(&self.spec, &self.snapshot.worker_sockets).await {
            Ok(child) => child,
            Err(error) => {
                self.spec.generation = old_generation;
                self.spec.control = old_control;
                self.spec.listener_socket = old_listener_socket;
                return Err(error);
            }
        };

        let old = std::mem::replace(&mut self.process, Process::Child(next));
        let snapshot = self.snapshot.clone();

        if let Err(error) = self.apply(snapshot).await {
            let failed = std::mem::replace(&mut self.process, old);
            let (control, listener_socket) = (
                std::mem::replace(&mut self.spec.control, old_control),
                std::mem::replace(&mut self.spec.listener_socket, old_listener_socket),
            );
            self.spec.generation = old_generation;
            tokio::spawn(finish(
                failed,
                control,
                listener_socket,
                Duration::from_secs(1),
            ));
            return Err(error);
        }

        let old_pid = pid_of(&old).unwrap_or(0);
        tokio::spawn(finish(old, old_control, old_listener_socket, grace));
        Ok(old_pid)
    }

    /// Tell the edge to stop and wait for it, killing it after a grace period.
    pub async fn stop(self) {
        finish(
            self.process,
            self.spec.control,
            self.spec.listener_socket,
            Duration::from_secs(10),
        )
        .await;
    }
}

/// A process's pid, if it still has one.
fn pid_of(process: &Process) -> Option<u32> {
    match process {
        Process::Child(child) => child.id(),
        Process::Adopted(pid) => Some(*pid),
    }
}

/// Start an edge from the spec and wait until its control socket answers.
async fn launch(spec: &EdgeSpec, worker_sockets: &[String]) -> anyhow::Result<Child> {
    let mut child = start(spec, worker_sockets)?;
    let started = Instant::now();

    while started.elapsed() < Duration::from_secs(30) {
        if child.try_wait()?.is_some() {
            anyhow::bail!(
                "the edge exited before it was ready; see {}",
                spec.log.display()
            );
        }

        if let Ok(mut client) = connect(&spec.control).await
            && client.get_status(pb::EdgeStatusRequest {}).await.is_ok()
        {
            return Ok(child);
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = child.kill().await;
    anyhow::bail!(
        "the edge did not answer on {} within 30 seconds",
        spec.control.display()
    )
}

/// Tell an edge to stop and wait for it, killing it after `grace`, then
/// remove its sockets (which a clean exit removes itself).
async fn finish(mut process: Process, control: PathBuf, listener_socket: PathBuf, grace: Duration) {
    if let Ok(mut client) = connect(&control).await {
        let _ = client.stop(pb::StopRequest {}).await;
    }

    match &mut process {
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

    let _ = std::fs::remove_file(&control);
    let _ = std::fs::remove_file(&listener_socket);
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
        .arg(spec.limits.max_headers.to_string())
        .arg("--max-connections")
        .arg(spec.limits.max_connections.to_string())
        .arg("--max-connections-per-client")
        .arg(spec.limits.max_connections_per_client.to_string())
        .arg("--max-requests-per-connection")
        .arg(spec.limits.max_requests_per_connection.to_string())
        .arg("--http2-max-concurrent-streams")
        .arg(spec.limits.http2_max_concurrent_streams.to_string());
    for (name, duration) in [
        ("queue", spec.limits.queue_timeout),
        ("deadline", spec.limits.request_deadline),
        ("header_read", spec.limits.header_read_timeout),
        ("body_idle", spec.limits.body_idle_timeout),
        ("response_idle", spec.limits.response_idle_timeout),
        ("keep_alive", spec.limits.keep_alive_timeout),
        ("keep_alive_proxy", spec.limits.keep_alive_timeout_proxy),
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
