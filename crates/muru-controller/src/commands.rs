//! Commands (design specification section 9.12): a process started with a
//! release's interpreter, environment, user and source directory, whose
//! output streams to the caller and into the release's log, and whose end
//! is recorded as an event.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use muru_proto::admin::v1 as pb;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::events::Events;
use crate::host::Identity;

/// How long a command told to stop, at its timeout or when its caller
/// goes away, gets before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// A command ready to start: everything the daemon resolved from the
/// release, so starting it needs nothing more from the daemon.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The application.
    pub app: String,
    /// The release it runs with.
    pub release: String,
    /// What the events and the log call it.
    pub label: String,
    /// The program and its arguments, run without a shell.
    pub argv: Vec<String>,
    /// The environment added to the daemon's own.
    pub env: Vec<(String, String)>,
    /// The directory it runs in: the release's source.
    pub cwd: PathBuf,
    /// Whom it runs as, when the daemon is root and the application
    /// names a user (section 10.9).
    pub identity: Option<Identity>,
    /// The release's log, which its output is appended to.
    pub log: PathBuf,
    /// How long it may run before it is stopped; no limit when absent.
    pub timeout: Option<Duration>,
}

impl Prepared {
    /// The subject of its events.
    fn subject(&self) -> String {
        format!("{}@{}", self.app, self.release)
    }
}

/// The release's interpreter as its host runs it.
pub struct Interpreter<'a> {
    /// The interpreter.
    pub python: &'a Path,
    /// The runtime library directory to put first on PYTHONPATH, when
    /// the environment's own `muru` is not used (section 8.3).
    pub runtime: Option<&'a Path>,
    /// Where bytecode is written, when it is redirected (section 8.2).
    pub pycache: Option<&'a Path>,
    /// Write no bytecode at all.
    pub no_bytecode: bool,
}

/// A command's environment: the release's, with its interpreter's
/// directory first on PATH (and VIRTUAL_ENV set when that is a virtual
/// environment), the runtime library first on PYTHONPATH, bytecode where
/// the host has it, and the MURU_ variables naming the application,
/// release and source, which the release's settings cannot override.
pub fn environment(
    mut env: Vec<(String, String)>,
    interpreter: &Interpreter<'_>,
    app: &str,
    release: &str,
    source: &Path,
) -> Vec<(String, String)> {
    let inherited = |env: &[(String, String)], name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
    };
    let first = |directory: &Path, rest: Option<String>| match rest {
        Some(rest) => format!("{}:{rest}", directory.display()),
        None => directory.display().to_string(),
    };

    let mut set = Vec::new();

    if let Some(bin) = interpreter.python.parent() {
        set.push(("PATH".to_owned(), first(bin, inherited(&env, "PATH"))));

        if let Some(root) = bin
            .parent()
            .filter(|root| root.join("pyvenv.cfg").is_file())
        {
            set.push(("VIRTUAL_ENV".to_owned(), root.display().to_string()));
        }
    }

    if let Some(runtime) = interpreter.runtime {
        set.push((
            "PYTHONPATH".to_owned(),
            first(runtime, inherited(&env, "PYTHONPATH")),
        ));
    }

    if let Some(prefix) = interpreter.pycache {
        set.push((
            "PYTHONPYCACHEPREFIX".to_owned(),
            prefix.display().to_string(),
        ));
    }

    if interpreter.no_bytecode {
        set.push(("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned()));
    }

    set.push(("MURU_APP".to_owned(), app.to_owned()));
    set.push(("MURU_RELEASE".to_owned(), release.to_owned()));
    set.push(("MURU_RELEASE_DIR".to_owned(), source.display().to_string()));

    env.retain(|(name, _)| !set.iter().any(|(key, _)| key == name) && !name.starts_with("MURU_"));
    env.extend(set);
    env
}

/// The command line with `python` or `python3` as its program replaced by
/// the release's interpreter, which a system interpreter's directory on
/// PATH would not otherwise guarantee.
pub fn with_interpreter(mut argv: Vec<String>, python: &Path) -> Vec<String> {
    if let Some(program) = argv.first_mut()
        && matches!(program.as_str(), "python" | "python3")
    {
        *program = python.display().to_string();
    }

    argv
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ending {
    /// The exit status, or 128 plus the signal that killed it.
    code: i32,
    /// How it ended, as a sentence.
    message: String,
    /// Whether it succeeded.
    ok: bool,
}

/// Start the command and stream what it writes. The receiver yields a
/// message per line of output and a last one saying how it ended; when
/// the receiver is dropped the command is stopped, as at its timeout.
pub fn run(prepared: Prepared, events: Events) -> mpsc::Receiver<pb::CommandOutput> {
    let (sender, receiver) = mpsc::channel(256);
    tokio::spawn(supervise(prepared, events, sender));
    receiver
}

/// Run the command to its end, relaying its output and recording it.
async fn supervise(prepared: Prepared, events: Events, sender: mpsc::Sender<pb::CommandOutput>) {
    let subject = prepared.subject();
    let started = Instant::now();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&prepared.log)
        .ok();

    let mut command = Command::new(&prepared.argv[0]);
    command
        .args(&prepared.argv[1..])
        .current_dir(&prepared.cwd)
        .envs(prepared.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // A session of its own, so stopping it reaches whatever it started,
    // as the user the release runs as.

    crate::host::own_session(&mut command, prepared.identity);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let ending = Ending {
                code: 127,
                message: format!("{} could not be started: {error}", prepared.argv[0]),
                ok: false,
            };
            events.emit(
                "command.failed",
                &subject,
                "warning",
                &format!("{}: {}", prepared.label, ending.message),
            );
            let _ = sender.send(finished(&prepared, &ending)).await;
            return;
        }
    };

    let pid = child.id().unwrap_or(0);
    events.emit(
        "command.started",
        &subject,
        "info",
        &format!("{}: started (pid {pid})", prepared.label),
    );

    // Both streams arrive as lines on one channel, in the order written.

    let (lines_tx, mut lines) = mpsc::channel::<(bool, String)>(256);

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(read_lines(stdout, false, lines_tx.clone()));
    }

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(read_lines(stderr, true, lines_tx.clone()));
    }

    drop(lines_tx);

    let deadline = prepared
        .timeout
        .map(|timeout| tokio::time::Instant::now() + timeout);
    let timed_out = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(timed_out);

    // Relay lines until both streams close, the command outlives its
    // timeout or the caller goes away.

    let mut stopped: Option<String> = None;
    let mut caller_gone = false;

    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some((stderr, line)) = line else {
                    break;
                };

                if let Some(log) = log.as_mut() {
                    let _ = writeln!(log, "[command {}] {line}", prepared.label);
                }

                let message = pb::CommandOutput {
                    release: prepared.release.clone(),
                    line,
                    stderr,
                    ..Default::default()
                };

                if !caller_gone && sender.send(message).await.is_err() {
                    caller_gone = true;

                    if stopped.is_none() {
                        stopped = Some("its caller went away".to_owned());
                        stop(pid, &mut child).await;
                    }
                }
            }
            () = &mut timed_out, if stopped.is_none() => {
                let timeout = prepared.timeout.unwrap_or_default();
                stopped = Some(format!(
                    "it ran past its timeout of {}",
                    muru_config::format_duration(timeout)
                ));
                stop(pid, &mut child).await;
            }
            () = sender.closed(), if !caller_gone => {
                caller_gone = true;

                if stopped.is_none() {
                    stopped = Some("its caller went away".to_owned());
                    stop(pid, &mut child).await;
                }
            }
        }
    }

    let ending = ending(child.wait().await, stopped, started.elapsed());

    if let Some(log) = log.as_mut() {
        let _ = writeln!(log, "[command {}] {}", prepared.label, ending.message);
    }

    let (kind, level) = if ending.ok {
        ("command.finished", "info")
    } else {
        ("command.failed", "warning")
    };
    events.emit(
        kind,
        &subject,
        level,
        &format!("{}: {}", prepared.label, ending.message),
    );

    if !caller_gone {
        let _ = sender.send(finished(&prepared, &ending)).await;
    }
}

/// Stop the command's session: SIGTERM, a grace period, then SIGKILL.
async fn stop(pid: u32, child: &mut tokio::process::Child) {
    if pid == 0 {
        let _ = child.start_kill();
        return;
    }

    crate::host::signal_group(pid, libc::SIGTERM);

    if tokio::time::timeout(STOP_GRACE, child.wait())
        .await
        .is_err()
    {
        crate::host::signal_group(pid, libc::SIGKILL);
    }
}

/// Send each line a stream carries, as text, until it closes.
async fn read_lines(
    stream: impl AsyncRead + Unpin,
    stderr: bool,
    lines: mpsc::Sender<(bool, String)>,
) {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();

        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buffer);
                let line = text.trim_end_matches(['\n', '\r']).to_owned();

                if lines.send((stderr, line)).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// How the command ended, from its exit status and whether it was
/// stopped, and why.
fn ending(
    status: std::io::Result<std::process::ExitStatus>,
    stopped: Option<String>,
    took: Duration,
) -> Ending {
    use std::os::unix::process::ExitStatusExt as _;

    // Milliseconds for a quick command, whole seconds for the rest.

    let after = if took < Duration::from_secs(1) {
        muru_config::format_duration(Duration::from_millis(took.as_millis() as u64))
    } else {
        muru_config::format_duration(Duration::from_secs(took.as_secs_f64().round() as u64))
    };

    let status = match status {
        Ok(status) => status,
        Err(error) => {
            return Ending {
                code: 1,
                message: format!("its exit status could not be read: {error}"),
                ok: false,
            };
        }
    };

    let (code, how) = match (status.code(), status.signal()) {
        (Some(code), _) => (code, format!("exited with status {code}")),
        (None, Some(signal)) => (128 + signal, format!("was killed by signal {signal}")),
        (None, None) => (1, "ended".to_owned()),
    };

    match stopped {
        Some(why) => Ending {
            code,
            message: format!("stopped because {why}; it {how} after {after}"),
            ok: false,
        },
        None => Ending {
            code,
            message: format!("{how} after {after}"),
            ok: code == 0,
        },
    }
}

/// The last message of a command's stream.
fn finished(prepared: &Prepared, ending: &Ending) -> pb::CommandOutput {
    pb::CommandOutput {
        release: prepared.release.clone(),
        finished: true,
        exit_code: ending.code,
        message: ending.message.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;
    use std::sync::Arc;

    use super::*;

    fn prepared(argv: &[&str], log: PathBuf) -> Prepared {
        Prepared {
            app: "shop".to_owned(),
            release: "r1".to_owned(),
            label: "exec".to_owned(),
            argv: argv.iter().map(|part| (*part).to_owned()).collect(),
            env: vec![("GREETING".to_owned(), "hello".to_owned())],
            cwd: std::env::temp_dir(),
            identity: None,
            log,
            timeout: None,
        }
    }

    async fn collect(mut receiver: mpsc::Receiver<pb::CommandOutput>) -> Vec<pb::CommandOutput> {
        let mut messages = Vec::new();

        while let Some(message) = receiver.recv().await {
            messages.push(message);
        }

        messages
    }

    #[test]
    fn the_environment_puts_the_interpreter_and_runtime_library_first() {
        let dir = tempfile::tempdir().expect("a directory");
        let bin = dir.path().join("venv").join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::write(dir.path().join("venv").join("pyvenv.cfg"), "").expect("pyvenv.cfg");
        let python = bin.join("python");
        let runtime = dir.path().join("runtime");
        let release = vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("PYTHONPATH".to_owned(), "/srv/lib".to_owned()),
            ("MURU_APP".to_owned(), "spoofed".to_owned()),
            ("DATABASE_URL".to_owned(), "sqlite://".to_owned()),
        ];
        let env = environment(
            release,
            &Interpreter {
                python: &python,
                runtime: Some(&runtime),
                pycache: None,
                no_bytecode: true,
            },
            "shop",
            "r2",
            Path::new("/srv/shop"),
        );
        let get = |name: &str| {
            env.iter()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>()
        };

        assert_eq!(get("PATH"), [format!("{}:/usr/bin", bin.display())]);
        assert_eq!(
            get("PYTHONPATH"),
            [format!("{}:/srv/lib", runtime.display())]
        );
        assert_eq!(
            get("VIRTUAL_ENV"),
            [dir.path().join("venv").display().to_string()]
        );
        assert_eq!(get("PYTHONDONTWRITEBYTECODE"), ["1"]);
        assert_eq!(get("MURU_APP"), ["shop"]);
        assert_eq!(get("MURU_RELEASE"), ["r2"]);
        assert_eq!(get("MURU_RELEASE_DIR"), ["/srv/shop"]);
        assert_eq!(get("DATABASE_URL"), ["sqlite://"]);
    }

    #[test]
    fn python_names_the_release_interpreter() {
        let python = Path::new("/srv/env/bin/python3.13");
        let argv = |parts: &[&str]| {
            parts
                .iter()
                .map(|part| (*part).to_owned())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            with_interpreter(argv(&["python", "manage.py", "migrate"]), python),
            argv(&["/srv/env/bin/python3.13", "manage.py", "migrate"])
        );
        assert_eq!(
            with_interpreter(argv(&["python3", "-V"]), python)[0],
            "/srv/env/bin/python3.13"
        );
        assert_eq!(
            with_interpreter(argv(&["flask", "routes"]), python)[0],
            "flask"
        );
    }

    #[test]
    fn an_ending_says_how_and_after_how_long() {
        let took = Duration::from_millis(125_400);
        let ok = ending(Ok(ExitStatus::from_raw(0)), None, took);
        assert_eq!((ok.code, ok.ok), (0, true));
        assert_eq!(
            ok.message,
            format!(
                "exited with status 0 after {}",
                muru_config::format_duration(Duration::from_secs(125))
            )
        );

        let quick = ending(Ok(ExitStatus::from_raw(0)), None, Duration::from_millis(42));
        assert_eq!(quick.message, "exited with status 0 after 42ms");

        let failed = ending(Ok(ExitStatus::from_raw(3 << 8)), None, took);
        assert_eq!((failed.code, failed.ok), (3, false));

        let killed = ending(
            Ok(ExitStatus::from_raw(libc::SIGTERM)),
            Some("it ran past its timeout of 1s".to_owned()),
            took,
        );
        assert_eq!(killed.code, 128 + libc::SIGTERM);
        assert!(!killed.ok);
        assert!(
            killed.message.starts_with(
                "stopped because it ran past its timeout of 1s; it was killed by signal"
            )
        );
    }

    #[tokio::test]
    async fn output_streams_in_lines_and_the_end_comes_last() {
        let dir = tempfile::tempdir().expect("a directory");
        let log = dir.path().join("r1.log");
        let events = Events::new(Arc::new(crate::store::Store::in_memory().expect("a store")));
        let receiver = run(
            prepared(
                &["sh", "-c", "echo $GREETING; echo oops >&2; exit 2"],
                log.clone(),
            ),
            events.clone(),
        );
        let messages = collect(receiver).await;

        let lines: Vec<(bool, &str)> = messages
            .iter()
            .filter(|message| !message.finished)
            .map(|message| (message.stderr, message.line.as_str()))
            .collect();
        assert!(lines.contains(&(false, "hello")), "{lines:?}");
        assert!(lines.contains(&(true, "oops")), "{lines:?}");

        let last = messages.last().expect("a last message");
        assert!(last.finished);
        assert_eq!(last.exit_code, 2);
        assert!(
            last.message.starts_with("exited with status 2 after"),
            "{}",
            last.message
        );

        let written = std::fs::read_to_string(&log).expect("the log");
        assert!(written.contains("[command exec] hello"), "{written}");
    }

    #[tokio::test]
    async fn a_command_past_its_timeout_is_stopped() {
        let dir = tempfile::tempdir().expect("a directory");
        let events = Events::new(Arc::new(crate::store::Store::in_memory().expect("a store")));
        let mut command = prepared(&["sleep", "30"], dir.path().join("r1.log"));
        command.timeout = Some(Duration::from_millis(200));
        let messages = collect(run(command, events)).await;

        let last = messages.last().expect("a last message");
        assert!(last.finished);
        assert_eq!(last.exit_code, 128 + libc::SIGTERM);
        assert!(
            last.message.contains("ran past its timeout"),
            "{}",
            last.message
        );
    }

    #[tokio::test]
    async fn a_program_that_is_not_found_ends_with_127() {
        let dir = tempfile::tempdir().expect("a directory");
        let events = Events::new(Arc::new(crate::store::Store::in_memory().expect("a store")));
        let messages = collect(run(
            prepared(&["no-such-program-anywhere"], dir.path().join("r1.log")),
            events,
        ))
        .await;

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].exit_code, 127);
        assert!(messages[0].message.contains("could not be started"));
    }
}
