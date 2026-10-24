//! The host process of one release in development mode: started with the
//! Python interpreter, watched until its workers listen, and stopped with
//! SIGTERM (design specification sections 6.1 and 13.1).

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use crate::logs::Logs;

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

        if let Some(dir) = &spec.chdir {
            command.current_dir(dir);
        }

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

/// Whether a process exists.
pub fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 checks for the process and sends nothing.

    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
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
