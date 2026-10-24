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
    /// Worker processes to fork.
    pub workers: usize,
    /// Application threads per worker.
    pub threads: usize,
    /// The preload mode: auto, true or false.
    pub preload: &'static str,
    /// The hooks module, if any.
    pub hooks: Option<String>,
    /// The directory to run the host in.
    pub chdir: Option<PathBuf>,
    /// Loopback TCP instead of Unix sockets for the link, from this port.
    pub tcp_base_port: Option<u16>,
}

/// A running host and the sockets its workers listen on.
pub struct HostProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The worker link sockets (paths, or addresses for the TCP link).
    pub sockets: Vec<PathBuf>,
    run_dir: PathBuf,
    /// The release number within this run, from 1.
    pub release: u32,
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
            .arg("--workers")
            .arg(spec.workers.to_string())
            .arg("--threads")
            .arg(spec.threads.to_string())
            .arg("--preload")
            .arg(spec.preload)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(hooks) = &spec.hooks {
            command.arg("--hooks").arg(hooks);
        }
        let base_port = spec
            .tcp_base_port
            .map(|base| base.wrapping_add((release - 1) as u16 * spec.workers as u16));
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
        let sockets = (0..spec.workers)
            .map(|index| match base_port {
                None => run_dir.join(format!("worker-{index}.sock")),
                Some(base) => {
                    let port = base.wrapping_add(u16::try_from(index).unwrap_or(0));
                    PathBuf::from(format!("127.0.0.1:{port}"))
                }
            })
            .collect();
        Ok(Self {
            child,
            stdin,
            sockets,
            run_dir,
            release,
        })
    }

    /// Wait until every worker listens, or the host exits or the time is up.
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let started = Instant::now();
        loop {
            if self.sockets.iter().all(|socket| listening(socket)) {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!("the host exited with {status} before its workers were ready");
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
        self.child.wait().await
    }

    /// Stop the host: SIGTERM and its stdin closed, then SIGKILL after a
    /// grace period. Its workers finish the requests they are serving.
    pub async fn stop(mut self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: kill(2) with a valid pid and signal has no memory-safety
            // preconditions; the pid belongs to the child we spawned.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        drop(self.stdin.take());
        if tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
        let _ = std::fs::remove_dir_all(&self.run_dir);
    }
}

fn listening(socket: &Path) -> bool {
    match socket.to_str() {
        Some(address) if address.starts_with("127.0.0.1:") => {
            std::net::TcpStream::connect(address).is_ok()
        }
        _ => socket.exists(),
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
