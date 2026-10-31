//! Managed HTTP server releases (design specification section 9.9): the
//! instances of a release that runs an ordinary HTTP server, which the
//! daemon starts, probes, supervises and stops, and which the edge
//! reaches as an upstream named after the release.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use muru_config::{ServerBind, ServerProfile};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UnixStream};
use tokio::process::{Child, Command};

use crate::host::{Identity, alive};
use crate::model::HttpServerSpec;

/// How long one readiness probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the instances are probed while they start.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// The wait before a crashed instance is started again, doubled with
/// each crash in a row up to the cap.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// The longest wait between a crash and the next start.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// An instance up this long has recovered: its next crash starts the
/// backoff over.
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// How long the instances get to be ready when the release says nothing.
pub const READINESS_TIMEOUT: Duration = Duration::from_secs(60);

/// Where one instance listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// A Unix socket.
    Unix(PathBuf),
    /// A loopback port.
    Port(u16),
}

impl Bind {
    /// The address as an upstream names it: `unix:/path` or
    /// `127.0.0.1:port`.
    pub fn address(&self) -> String {
        match self {
            Self::Unix(path) => format!("unix:{}", path.display()),
            Self::Port(port) => format!("127.0.0.1:{port}"),
        }
    }

    /// The address parsed back from a record.
    fn parse(text: &str) -> Option<Self> {
        if let Some(path) = text.strip_prefix("unix:") {
            return Some(Self::Unix(PathBuf::from(path)));
        }

        text.strip_prefix("127.0.0.1:")
            .and_then(|port| port.parse().ok())
            .map(Self::Port)
    }
}

/// A process this daemon started, or one it adopted from a previous
/// daemon.
enum Process {
    Child(Child),
    Adopted(u32),
}

/// One supervised instance.
struct Instance {
    index: u32,
    bind: Bind,
    process: Option<Process>,
    /// When the process was started, for the backoff reset.
    started: Instant,
    /// Crashes in a row, each doubling the wait before the next start.
    crashes: u32,
    /// When the next start is due, after a crash.
    due: Option<Instant>,
}

impl Instance {
    fn pid(&self) -> Option<u32> {
        match &self.process {
            Some(Process::Child(child)) => child.id(),
            Some(Process::Adopted(pid)) => Some(*pid),
            None => None,
        }
    }

    /// Whether the process has exited, without waiting: a sentence
    /// saying how when it has.
    fn try_exited(&mut self) -> std::io::Result<Option<String>> {
        match &mut self.process {
            Some(Process::Child(child)) => Ok(child
                .try_wait()?
                .map(|status| format!("exited with {status}"))),
            Some(Process::Adopted(pid)) => Ok((!alive(*pid)).then(|| "is gone".to_owned())),
            None => Ok(None),
        }
    }

    fn signal(&self, signal: i32) {
        if let Some(pid) = self.pid() {
            // SAFETY: kill(2) with a pid this daemon started or adopted.

            unsafe {
                libc::kill(pid as libc::pid_t, signal);
            }
        }
    }

    /// Wait for the process to exit until the deadline, then kill it.
    async fn finish(&mut self, deadline: Instant) {
        match &mut self.process {
            Some(Process::Child(child)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());

                if tokio::time::timeout(remaining, child.wait()).await.is_err() {
                    let _ = child.kill().await;
                }
            }
            Some(Process::Adopted(pid)) => {
                let pid = *pid;

                while alive(pid) && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }

                if alive(pid) {
                    // SAFETY: as in signal.

                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
            }
            None => {}
        }

        self.process = None;

        if let Bind::Unix(path) = &self.bind {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A change the supervisor reports, for the event stream.
pub struct Report {
    /// The event type.
    pub kind: &'static str,
    /// Its level.
    pub level: &'static str,
    /// What happened.
    pub message: String,
}

/// What a previous daemon recorded about the instances, in the
/// release's run directory.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ServersRecord {
    /// Each instance's pid and address.
    pub instances: Vec<InstanceRecord>,
}

/// One instance as recorded.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct InstanceRecord {
    /// The instance's index, from 0.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// Where it listens, as an upstream names it.
    pub address: String,
}

/// The instances of one managed HTTP release.
pub struct Servers {
    spec: HttpServerSpec,
    run_dir: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    identity: Option<Identity>,
    log: PathBuf,
    instances: Vec<Instance>,
}

impl Servers {
    /// Start every instance under the run directory, each with its own
    /// address, running `spec.command` in `cwd` with `env`, as
    /// `identity` when one is set, its output appended to `log`.
    pub fn start(
        spec: &HttpServerSpec,
        run_dir: &Path,
        cwd: &Path,
        env: Vec<(String, String)>,
        identity: Option<Identity>,
        log: &Path,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(run_dir)
            .with_context(|| format!("creating {}", run_dir.display()))?;
        let mut servers = Self {
            spec: spec.clone(),
            run_dir: run_dir.to_path_buf(),
            cwd: cwd.to_path_buf(),
            env,
            identity,
            log: log.to_path_buf(),
            instances: Vec::new(),
        };

        for index in 0..spec.instances.max(1) {
            let bind = servers.allocate(index)?;
            servers.instances.push(Instance {
                index,
                bind,
                process: None,
                started: Instant::now(),
                crashes: 0,
                due: None,
            });
        }

        for position in 0..servers.instances.len() {
            servers.spawn(position)?;
        }

        Ok(servers)
    }

    /// Take over the instances a previous daemon recorded: each must
    /// still be there. They are stopped with signals rather than waited
    /// for, and one that exits is started again as a child of this
    /// daemon.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt(
        spec: &HttpServerSpec,
        run_dir: &Path,
        cwd: &Path,
        env: Vec<(String, String)>,
        identity: Option<Identity>,
        log: &Path,
        record: &ServersRecord,
    ) -> anyhow::Result<Self> {
        let mut instances = Vec::new();

        for instance in &record.instances {
            anyhow::ensure!(
                alive(instance.pid),
                "instance {} (pid {}) is gone",
                instance.index,
                instance.pid
            );
            let bind = Bind::parse(&instance.address).with_context(|| {
                format!(
                    "instance {}: {:?} is not an address",
                    instance.index, instance.address
                )
            })?;
            instances.push(Instance {
                index: instance.index,
                bind,
                process: Some(Process::Adopted(instance.pid)),
                started: Instant::now(),
                crashes: 0,
                due: None,
            });
        }

        anyhow::ensure!(!instances.is_empty(), "no instances were recorded");

        Ok(Self {
            spec: spec.clone(),
            run_dir: run_dir.to_path_buf(),
            cwd: cwd.to_path_buf(),
            env,
            identity,
            log: log.to_path_buf(),
            instances,
        })
    }

    /// A socket path or a free loopback port for an instance.
    fn allocate(&self, index: u32) -> anyhow::Result<Bind> {
        match self.spec.bind {
            ServerBind::Unix => Ok(Bind::Unix(
                self.run_dir.join(format!("server-{index}.sock")),
            )),
            ServerBind::Port => {
                let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
                    .context("finding a free loopback port")?;
                Ok(Bind::Port(listener.local_addr()?.port()))
            }
        }
    }

    /// Start one instance's process.
    fn spawn(&mut self, position: usize) -> anyhow::Result<()> {
        let instance = &self.instances[position];

        // A crashed instance leaves its socket file behind, which the
        // next one could not bind.

        if let Bind::Unix(path) = &instance.bind {
            let _ = std::fs::remove_file(path);
        }

        let arguments = arguments_for(self.spec.profile, &self.spec.command, &instance.bind);
        let program = Path::new(&arguments[0]);

        // A relative path with a directory in it is taken from the
        // working directory, which is what an operator writing
        // `.venv/bin/gunicorn` means; a bare name is found on PATH.

        let program = if program.is_relative() && program.components().count() > 1 {
            self.cwd.join(program)
        } else {
            program.to_path_buf()
        };

        // Output goes to the release's log file itself, not through this
        // daemon, so a daemon that detaches or upgrades leaves the
        // instance writing; the file is rotated in place (section 11.3).

        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .with_context(|| format!("opening {}", self.log.display()))?;
        let mut command = Command::new(&program);
        command
            .args(&arguments[1..])
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .envs(self.env.iter().cloned())
            .env("MURU_HTTP_BIND", instance.bind.address())
            .env("MURU_INSTANCE", instance.index.to_string());

        if let Bind::Port(port) = instance.bind {
            command.env("PORT", port.to_string());
        }

        crate::host::own_session(&mut command, self.identity);
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        let instance = &mut self.instances[position];
        instance.process = Some(Process::Child(child));
        instance.started = Instant::now();
        instance.due = None;
        Ok(())
    }

    /// Wait until every instance answers its readiness probe, or one
    /// exits, or the time is up.
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut ready = vec![false; self.instances.len()];
        let path = self.spec.readiness_path.clone();

        loop {
            for (position, instance) in self.instances.iter_mut().enumerate() {
                if ready[position] {
                    continue;
                }

                if let Some(how) = instance.try_exited()? {
                    anyhow::bail!("instance {} {how} before it was ready", instance.index);
                }

                if probe(&instance.bind, path.as_deref(), PROBE_TIMEOUT).await {
                    ready[position] = true;
                }
            }

            if ready.iter().all(|ready| *ready) {
                return Ok(());
            }

            if Instant::now() >= deadline {
                let waiting = self
                    .instances
                    .iter()
                    .zip(&ready)
                    .find(|(_, ready)| !**ready)
                    .map(|(instance, _)| instance.index)
                    .unwrap_or(0);
                anyhow::bail!(
                    "instance {waiting} was not ready within {} s{}",
                    timeout.as_secs(),
                    path.as_deref()
                        .map(|path| format!(": {path} did not answer 2xx or 3xx"))
                        .unwrap_or_default()
                );
            }

            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    }

    /// Reap instances that exited and start again those whose wait has
    /// passed, with what happened for the event stream.
    pub fn check(&mut self) -> Vec<Report> {
        let now = Instant::now();
        let mut reports = Vec::new();

        for position in 0..self.instances.len() {
            let instance = &mut self.instances[position];

            if let Ok(Some(how)) = instance.try_exited() {
                instance.process = None;

                if instance.started.elapsed() >= STABLE_AFTER {
                    instance.crashes = 0;
                }

                instance.crashes += 1;
                let wait = backoff(instance.crashes);
                instance.due = Some(now + wait);
                reports.push(Report {
                    kind: "server.exited",
                    level: "warning",
                    message: format!(
                        "instance {} {how}; starting it again in {}",
                        instance.index,
                        muru_config::format_duration(wait)
                    ),
                });
            }

            if instance.process.is_some() || !instance.due.is_some_and(|due| due <= now) {
                continue;
            }

            let index = instance.index;

            match self.spawn(position) {
                Ok(()) => reports.push(Report {
                    kind: "server.started",
                    level: "info",
                    message: format!(
                        "instance {index} started again (pid {})",
                        self.instances[position].pid().unwrap_or(0)
                    ),
                }),
                Err(error) => {
                    let instance = &mut self.instances[position];
                    instance.crashes += 1;
                    let wait = backoff(instance.crashes);
                    instance.due = Some(now + wait);
                    reports.push(Report {
                        kind: "server.failed",
                        level: "error",
                        message: format!(
                            "instance {index} could not be started: {error:#}; trying again in {}",
                            muru_config::format_duration(wait)
                        ),
                    });
                }
            }
        }

        reports
    }

    /// Every instance's address, as the snapshot names them.
    pub fn addresses(&self) -> Vec<String> {
        self.instances
            .iter()
            .map(|instance| instance.bind.address())
            .collect()
    }

    /// How many instances have a process right now.
    pub fn running(&self) -> usize {
        self.instances
            .iter()
            .filter(|instance| instance.process.is_some())
            .count()
    }

    /// The instances as a record for the next daemon.
    pub fn record(&self) -> ServersRecord {
        ServersRecord {
            instances: self
                .instances
                .iter()
                .map(|instance| InstanceRecord {
                    index: instance.index,
                    pid: instance.pid().unwrap_or(0),
                    address: instance.bind.address(),
                })
                .collect(),
        }
    }

    /// The signal that stops an instance gracefully: the profile's, or
    /// the one the release names.
    fn stop_signal(&self) -> i32 {
        let name = self
            .spec
            .stop_signal
            .as_deref()
            .unwrap_or(match self.spec.profile {
                ServerProfile::Gunicorn
                | ServerProfile::Uvicorn
                | ServerProfile::Granian
                | ServerProfile::ModWsgiExpress
                | ServerProfile::Generic => "TERM",
            });

        match name {
            "INT" => libc::SIGINT,
            "QUIT" => libc::SIGQUIT,
            "HUP" => libc::SIGHUP,
            "USR1" => libc::SIGUSR1,
            "USR2" => libc::SIGUSR2,
            "WINCH" => libc::SIGWINCH,
            _ => libc::SIGTERM,
        }
    }

    /// Stop every instance: the graceful signal, then SIGKILL for one
    /// still there after `grace`, and the sockets and run directory
    /// removed.
    pub async fn stop(self, grace: Duration) {
        self.stop_until(Instant::now() + grace).await;
    }

    /// Stop the instances as `stop` does, killing what is still there at
    /// `kill_at`.
    pub async fn stop_until(mut self, kill_at: Instant) {
        let signal = self.stop_signal();

        for instance in &self.instances {
            instance.signal(signal);
        }

        let deadline = kill_at;

        for instance in &mut self.instances {
            instance.finish(deadline).await;
        }

        let _ = std::fs::remove_dir_all(&self.run_dir);
    }

    /// Leave the instances running and forget them, for a daemon that
    /// detaches.
    pub fn release(self) {
        for instance in self.instances {
            if let Some(Process::Child(mut child)) = instance.process {
                // The child must not be killed when its handle drops.

                let _ = child.try_wait();
                std::mem::forget(child);
            }
        }
    }
}

/// The wait before the next start after this many crashes in a row.
fn backoff(crashes: u32) -> Duration {
    BACKOFF_FIRST
        .saturating_mul(1 << crashes.saturating_sub(1).min(5))
        .min(BACKOFF_CAP)
}

/// The command with its address filled in: `{bind}` becomes the
/// address in the server's own form, `{socket}` the socket path and
/// `{port}` the port, and a command that names none of them gets the
/// profile's own flags appended.
fn arguments_for(profile: ServerProfile, command: &[String], bind: &Bind) -> Vec<String> {
    let address = match (profile, bind) {
        (ServerProfile::Uvicorn | ServerProfile::Granian, Bind::Unix(path)) => {
            path.display().to_string()
        }
        (_, bind) => bind.address(),
    };

    let socket = match bind {
        Bind::Unix(path) => path.display().to_string(),
        Bind::Port(_) => String::new(),
    };

    let port = match bind {
        Bind::Port(port) => port.to_string(),
        Bind::Unix(_) => String::new(),
    };

    let mentioned = command.iter().any(|argument| {
        argument.contains("{bind}") || argument.contains("{socket}") || argument.contains("{port}")
    });
    let mut arguments: Vec<String> = command
        .iter()
        .map(|argument| {
            argument
                .replace("{bind}", &address)
                .replace("{socket}", &socket)
                .replace("{port}", &port)
        })
        .collect();

    if mentioned {
        return arguments;
    }

    match (profile, bind) {
        (ServerProfile::Gunicorn, _) => arguments.extend(["--bind".to_owned(), address]),
        (ServerProfile::Uvicorn | ServerProfile::Granian, Bind::Unix(_)) => {
            arguments.extend(["--uds".to_owned(), socket]);
        }
        (
            ServerProfile::Uvicorn | ServerProfile::Granian | ServerProfile::ModWsgiExpress,
            Bind::Port(_),
        ) => {
            arguments.extend([
                "--host".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                port,
            ]);
        }
        (ServerProfile::ModWsgiExpress, Bind::Unix(_)) | (ServerProfile::Generic, _) => {}
    }

    arguments
}

/// Whether an instance answers: with a path, a GET of it answered 2xx
/// or 3xx; without one, any HTTP response at all.
async fn probe(bind: &Bind, path: Option<&str>, timeout: Duration) -> bool {
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: localhost\r\nUser-Agent: muru-readiness\r\n\r\n",
        path.unwrap_or("/")
    );
    let answered = tokio::time::timeout(timeout, async {
        match bind {
            Bind::Unix(socket) => {
                let mut stream = UnixStream::connect(socket).await.ok()?;
                stream.write_all(request.as_bytes()).await.ok()?;
                status_of(&mut stream).await
            }
            Bind::Port(port) => {
                let mut stream = TcpStream::connect(("127.0.0.1", *port)).await.ok()?;
                stream.write_all(request.as_bytes()).await.ok()?;
                status_of(&mut stream).await
            }
        }
    })
    .await;

    match answered {
        Ok(Some(status)) => path.is_none() || (200..400).contains(&status),
        _ => false,
    }
}

/// The status of the response a stream carries, from its status line.
async fn status_of<S: AsyncRead + Unpin>(stream: &mut S) -> Option<u16> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];

    while head.len() < 256 {
        if stream.read(&mut byte).await.ok()? == 0 {
            break;
        }

        if byte[0] == b'\n' {
            break;
        }

        head.push(byte[0]);
    }

    let line = String::from_utf8_lossy(&head);
    let mut words = line.split_whitespace();
    let version = words.next()?;

    if !version.starts_with("HTTP/") {
        return None;
    }

    words.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn placeholders_take_the_address_in_the_servers_form() {
        let unix = Bind::Unix(PathBuf::from("/run/s.sock"));
        let port = Bind::Port(8001);
        assert_eq!(
            arguments_for(
                ServerProfile::Gunicorn,
                &command(&["gunicorn", "--bind", "{bind}", "app:app"]),
                &unix
            ),
            command(&["gunicorn", "--bind", "unix:/run/s.sock", "app:app"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Uvicorn,
                &command(&["uvicorn", "app:app", "--uds", "{bind}"]),
                &unix
            ),
            command(&["uvicorn", "app:app", "--uds", "/run/s.sock"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Generic,
                &command(&["server", "--listen={bind}", "--port", "{port}"]),
                &port
            ),
            command(&["server", "--listen=127.0.0.1:8001", "--port", "8001"])
        );
    }

    #[test]
    fn a_command_without_placeholders_gets_the_profiles_flags() {
        let unix = Bind::Unix(PathBuf::from("/run/s.sock"));
        let port = Bind::Port(8001);
        assert_eq!(
            arguments_for(
                ServerProfile::Gunicorn,
                &command(&["gunicorn", "app:app"]),
                &port
            ),
            command(&["gunicorn", "app:app", "--bind", "127.0.0.1:8001"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Uvicorn,
                &command(&["uvicorn", "app:app"]),
                &unix
            ),
            command(&["uvicorn", "app:app", "--uds", "/run/s.sock"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Granian,
                &command(&["granian", "app:app"]),
                &port
            ),
            command(&[
                "granian",
                "app:app",
                "--host",
                "127.0.0.1",
                "--port",
                "8001"
            ])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::ModWsgiExpress,
                &command(&["mod_wsgi-express", "start-server", "app.wsgi"]),
                &port
            ),
            command(&[
                "mod_wsgi-express",
                "start-server",
                "app.wsgi",
                "--host",
                "127.0.0.1",
                "--port",
                "8001"
            ])
        );
        assert_eq!(
            arguments_for(ServerProfile::Generic, &command(&["server"]), &port),
            command(&["server"])
        );
    }

    #[test]
    fn the_backoff_doubles_to_the_cap() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(4), Duration::from_secs(8));
        assert_eq!(backoff(9), Duration::from_secs(30));
    }

    #[test]
    fn addresses_round_trip_through_records() {
        for bind in [Bind::Unix(PathBuf::from("/run/s.sock")), Bind::Port(8001)] {
            assert_eq!(Bind::parse(&bind.address()), Some(bind));
        }

        assert_eq!(Bind::parse("10.0.0.1:80"), None);
    }

    #[tokio::test]
    async fn the_probe_reads_a_status_line() {
        let mut stream = std::io::Cursor::new(b"HTTP/1.1 503 Service Unavailable\r\n\r\n".to_vec());
        assert_eq!(status_of(&mut stream).await, Some(503));
        let mut stream = std::io::Cursor::new(b"nonsense\r\n".to_vec());
        assert_eq!(status_of(&mut stream).await, None);
    }
}
