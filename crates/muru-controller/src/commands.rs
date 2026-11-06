//! Commands (design specification section 9.12): a process started with a
//! release's interpreter, environment, user and source directory, whose
//! output streams to the caller and into the release's log, and whose end
//! is recorded as an event. The daemon runs them for a release; a
//! development run, `muructl serve` and muructl on its own run them for
//! the project they serve or stand in, each recording a run its own way.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use muru_proto::admin::v1 as pb;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::events::Events;
use crate::host::Identity;
use crate::model::{CommandRun, CommandSpec, now_unix_ms};
use crate::store::Store;

/// How long a command told to stop, at its timeout or when its caller
/// goes away, gets before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Where a run of a command is recorded, beyond the stream its caller
/// reads. The daemon keeps events and each command's last run; a
/// development run and `muructl serve` write to their own output; a
/// command muructl runs on its own records nothing.
pub trait Record: Send + Sync {
    /// A line the command wrote, or the last one saying how it ended.
    fn output(&self, _label: &str, _line: &str, _stderr: bool) {}

    /// The run started, finished or failed.
    fn event(&self, _kind: &str, _subject: &str, _level: &'static str, _message: &str) {}

    /// A declared command's run ended.
    fn ended(&self, _app: &str, _name: &str, _run: &CommandRun) {}
}

/// The daemon's record: events in the store and on the stream, and the
/// last run of each declared command.
pub struct Recorded {
    /// The event stream.
    pub events: Events,
    /// The store.
    pub store: Arc<Store>,
}

impl Record for Recorded {
    fn event(&self, kind: &str, subject: &str, level: &'static str, message: &str) {
        self.events.emit(kind, subject, level, message);
    }

    fn ended(&self, app: &str, name: &str, run: &CommandRun) {
        if let Err(error) = self.store.record_command_run(app, name, run) {
            tracing::warn!(%error, "recording the run of {name} failed");
        }
    }
}

/// No record at all: the caller's stream is everything.
pub struct Unrecorded;

impl Record for Unrecorded {}

/// What a caller asks to run.
#[derive(Debug, Clone)]
pub enum CommandRequest {
    /// A one-off command: the program and its arguments.
    OneOff(Vec<String>),
    /// A command the release declares, with its inputs.
    Declared {
        /// Its name.
        name: String,
        /// Input values given.
        inputs: BTreeMap<String, String>,
        /// Inputs taken from a secret store, as STORE/KEY.
        secret_inputs: BTreeMap<String, String>,
    },
}

impl CommandRequest {
    /// What an admin API request asks to run: a declared command by
    /// name, or a one-off one, and not both.
    pub fn from_message(request: pb::RunCommandRequest) -> Result<Self, &'static str> {
        match (request.name.is_empty(), request.argv.is_empty()) {
            (true, false) => Ok(Self::OneOff(request.argv)),
            (false, true) => Ok(Self::Declared {
                name: request.name,
                inputs: request.inputs.into_iter().collect(),
                secret_inputs: request.secret_inputs.into_iter().collect(),
            }),
            (true, true) => Err("name the command to run"),
            (false, false) => Err("name a declared command or give a one-off one, not both"),
        }
    }
}

/// The declared commands running now that run one at a time, by
/// application and name.
static EXCLUSIVE: LazyLock<Mutex<BTreeSet<(String, String)>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));

/// A claim on an exclusive command, given up when dropped.
struct Claim(Option<(String, String)>);

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(key) = self.0.take() {
            EXCLUSIVE
                .lock()
                .expect("exclusive commands poisoned")
                .remove(&key);
        }
    }
}

/// A run's own secret files, removed when it ends, however it ends, with
/// the directory of the release's command runs when that leaves it empty.
struct SecretFiles(Option<PathBuf>);

impl Drop for SecretFiles {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(&dir);

            if let Some(runs) = dir.parent() {
                let _ = std::fs::remove_dir(runs);
            }
        }
    }
}

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
    /// The release's log, which its output is appended to; none where
    /// the record takes the output instead.
    pub log: Option<PathBuf>,
    /// How long it may run before it is stopped; no limit when absent.
    pub timeout: Option<Duration>,
    /// The declared command's name, whose last run is recorded; none for
    /// a one-off command.
    pub declared: Option<String>,
    /// Whether it runs one at a time for the application.
    pub exclusive: bool,
    /// Its inputs as its events show them.
    pub inputs: String,
    /// The directory of its own secret files (section 10.8), removed when
    /// it ends.
    pub secrets: Option<PathBuf>,
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

    // A command run for a project on its own has no release to name.

    if !release.is_empty() {
        set.push(("MURU_RELEASE".to_owned(), release.to_owned()));
    }

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

/// The declared command of that name, or why there is none.
pub fn find<'a>(
    commands: &'a [CommandSpec],
    name: &str,
    subject: &str,
) -> Result<&'a CommandSpec, String> {
    commands
        .iter()
        .find(|command| command.name == name)
        .ok_or_else(|| {
            let names: Vec<&str> = commands
                .iter()
                .map(|command| command.name.as_str())
                .collect();

            let declared = if names.is_empty() {
                "it declares none".to_owned()
            } else {
                format!("it declares {}", names.join(", "))
            };
            format!("{subject} has no command named {name}; {declared}")
        })
}

/// A declared command ready to start, from what every command of the
/// release runs with: its program with the inputs in place, or the
/// runtime library's runner calling its callable with them as keyword
/// arguments (those given or with a default, in MURU_COMMAND_INPUTS),
/// then its own variables over the release's and MURU_COMMAND.
pub fn declared(
    prepared: Prepared,
    command: &CommandSpec,
    given: &BTreeMap<String, String>,
    values: &BTreeMap<String, String>,
    python: &Path,
) -> Result<Prepared, String> {
    let mut env = prepared.env.clone();

    let argv = match &command.call {
        Some(target) => {
            let keywords: BTreeMap<&String, &String> = values
                .iter()
                .filter(|(name, _)| {
                    given.contains_key(*name)
                        || command
                            .inputs
                            .iter()
                            .any(|input| &input.name == *name && input.default.is_some())
                })
                .collect();
            env.push((
                "MURU_COMMAND_INPUTS".to_owned(),
                serde_json::to_string(&keywords).map_err(|error| error.to_string())?,
            ));
            vec![
                python.display().to_string(),
                "-m".to_owned(),
                "muru._command".to_owned(),
                target.clone(),
            ]
        }
        None => with_interpreter(
            command
                .run
                .iter()
                .map(|argument| substitute(argument, values))
                .collect::<Result<_, _>>()?,
            python,
        ),
    };

    for (variable, template) in &command.env {
        let value = substitute(template, values)?;
        env.retain(|(name, _)| name != variable);
        env.push((variable.clone(), value));
    }

    env.push(("MURU_COMMAND".to_owned(), command.name.clone()));

    Ok(Prepared {
        label: command.name.clone(),
        argv,
        env,
        timeout: command.timeout_ms.map(Duration::from_millis),
        declared: Some(command.name.clone()),
        exclusive: command.exclusive,
        inputs: describe_inputs(command, values),
        ..prepared
    })
}

/// The inputs a template names, in order: `{name}` is an input, `{{`
/// and `}}` are literal braces, and anything else in braces is refused.
pub fn placeholders(template: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    expand(template, |name| {
        names.push(name.to_owned());
        Ok(String::new())
    })?;
    Ok(names)
}

/// The template with each `{name}` replaced by the input's value.
pub fn substitute(template: &str, values: &BTreeMap<String, String>) -> Result<String, String> {
    expand(template, |name| {
        values
            .get(name)
            .cloned()
            .ok_or_else(|| format!("{{{name}}} names no input"))
    })
}

/// Walk a template, replacing each placeholder with what `value` gives.
fn expand(
    template: &str,
    mut value: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(at) = rest.find(['{', '}']) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];

        if let Some(after) = tail.strip_prefix("{{") {
            out.push('{');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("}}") {
            out.push('}');
            rest = after;
        } else if let Some(after) = tail.strip_prefix('{') {
            let Some(end) = after.find('}') else {
                return Err(format!(
                    "{template:?} has a {{ with no }}; write {{{{ for a brace"
                ));
            };
            let name = &after[..end];

            if !crate::model::valid_env_name(name) {
                return Err(format!(
                    "{template:?} has {{{name}}}, which is not an input name; write {{{{ for a brace"
                ));
            }

            out.push_str(&value(name)?);
            rest = &after[end + 1..];
        } else {
            return Err(format!(
                "{template:?} has a }} with no {{; write }}}} for a brace"
            ));
        }
    }

    out.push_str(rest);
    Ok(out)
}

/// The value of each input for a run: those given, checked against the
/// declaration, then the defaults. An input that is not declared, a
/// required one missing or a value that does not match its pattern is
/// refused, naming the input but never a secret's value.
pub fn input_values(
    command: &CommandSpec,
    given: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    if let Some(unknown) = given
        .keys()
        .find(|name| !command.inputs.iter().any(|input| &input.name == *name))
    {
        let known: Vec<&str> = command
            .inputs
            .iter()
            .map(|input| input.name.as_str())
            .collect();

        let known = if known.is_empty() {
            "it takes none".to_owned()
        } else {
            format!("it takes {}", known.join(", "))
        };
        return Err(format!(
            "{} has no input named {unknown}; {known}",
            command.name
        ));
    }

    let mut values = BTreeMap::new();

    for input in &command.inputs {
        let Some(value) = given.get(&input.name).or(input.default.as_ref()) else {
            if input.required {
                return Err(format!("{} needs the input {}", command.name, input.name));
            }

            values.insert(input.name.clone(), String::new());
            continue;
        };

        if let Some(pattern) = &input.pattern {
            let anchored = regex::Regex::new(&format!("^(?:{pattern})$"))
                .map_err(|error| format!("{}: input {}: {error}", command.name, input.name))?;

            if !anchored.is_match(value) {
                let shown = if input.secret {
                    "the value given".to_owned()
                } else {
                    format!("{value:?}")
                };
                return Err(format!(
                    "{} input {}: {shown} does not match {pattern}",
                    command.name, input.name
                ));
            }
        }

        values.insert(input.name.clone(), value.clone());
    }

    Ok(values)
}

/// The inputs as an event shows them: each name with its value, a
/// secret's as `***`.
pub fn describe_inputs(command: &CommandSpec, values: &BTreeMap<String, String>) -> String {
    command
        .inputs
        .iter()
        .filter_map(|input| {
            let value = values.get(&input.name)?;
            let shown = if input.secret { "***" } else { value.as_str() };
            Some(format!("{}={shown}", input.name))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A declared command as the admin API lists it, with its last run when
/// one is kept.
pub fn info(command: &CommandSpec, last_run: Option<&CommandRun>) -> pb::CommandInfo {
    pb::CommandInfo {
        name: command.name.clone(),
        description: command.description.clone(),
        run: command.run.clone(),
        call: command.call.clone().unwrap_or_default(),
        inputs: command
            .inputs
            .iter()
            .map(|input| pb::CommandInputInfo {
                name: input.name.clone(),
                description: input.description.clone(),
                required: input.required,
                default: input.default.clone().unwrap_or_default(),
                pattern: input.pattern.clone().unwrap_or_default(),
                secret: input.secret,
            })
            .collect(),
        timeout: command
            .timeout_ms
            .map(|ms| muru_config::format_duration(Duration::from_millis(ms)))
            .unwrap_or_default(),
        exclusive: command.exclusive,
        last_run: last_run.map(|run| pb::CommandRun {
            release: run.release.clone(),
            started_unix_ms: run.started_unix_ms,
            duration_ms: run.duration_ms,
            exit_code: run.exit_code,
            ok: run.ok,
            message: run.message.clone(),
        }),
    }
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

/// A command `run` started: what it writes, and a way to stop it while
/// that is still read.
pub struct Running {
    /// A message per line of output, and a last one saying how it ended.
    /// When it is dropped the command is stopped, as at its timeout,
    /// because its caller went away.
    pub output: mpsc::Receiver<pb::CommandOutput>,
    /// Stops the command for a caller that reads on to its end.
    pub stopper: Stopper,
}

/// What a caller asks of a command whose output it still reads.
enum Request {
    /// Stop it as at its timeout, for this reason.
    Stop(String),
    /// Kill its session at once.
    Kill,
}

/// Stops a command `run` started, while its caller goes on reading the
/// output, so the last message still says how it ended. Dropping it
/// leaves the command running.
#[derive(Clone)]
pub struct Stopper(mpsc::UnboundedSender<Request>);

impl Stopper {
    /// Stop the command as at its timeout: its session gets SIGTERM and,
    /// after the grace period, SIGKILL. `why` is how its end says why it
    /// was stopped, such as "muructl got SIGTERM".
    pub fn stop(&self, why: impl Into<String>) {
        let _ = self.0.send(Request::Stop(why.into()));
    }

    /// Kill the command's session at once, during the grace period of a
    /// stop or in place of one.
    pub fn kill(&self) {
        let _ = self.0.send(Request::Kill);
    }
}

/// Start the command and stream what it writes. An exclusive command
/// already running for the application is refused.
pub fn run(prepared: Prepared, record: Arc<dyn Record>) -> Result<Running, String> {
    let secrets = SecretFiles(prepared.secrets.clone());

    let claim = match (&prepared.declared, prepared.exclusive) {
        (Some(name), true) => {
            let key = (prepared.app.clone(), name.clone());

            if !EXCLUSIVE
                .lock()
                .expect("exclusive commands poisoned")
                .insert(key.clone())
            {
                return Err(format!(
                    "{name} runs one at a time for {}, and a run is under way",
                    prepared.app
                ));
            }

            Claim(Some(key))
        }
        _ => Claim(None),
    };

    let (sender, output) = mpsc::channel(256);
    let (stopper, requests) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        supervise(prepared, record, sender, requests).await;
        drop(claim);
        drop(secrets);
    });

    Ok(Running {
        output,
        stopper: Stopper(stopper),
    })
}

/// Run the command to its end, relaying its output and recording it.
async fn supervise(
    prepared: Prepared,
    record: Arc<dyn Record>,
    sender: mpsc::Sender<pb::CommandOutput>,
    mut requests: mpsc::UnboundedReceiver<Request>,
) {
    let subject = prepared.subject();
    let started = Instant::now();
    let started_unix_ms = now_unix_ms();
    let mut log = prepared.log.as_ref().and_then(|log| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .ok()
    });

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
            record.event(
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

    let with = if prepared.inputs.is_empty() {
        String::new()
    } else {
        format!(", with {}", prepared.inputs)
    };
    record.event(
        "command.started",
        &subject,
        "info",
        &format!("{}: started (pid {pid}){with}", prepared.label),
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

    // Relay lines until both streams close. The command is stopped when
    // it outlives its timeout, when the caller asks or when the caller
    // goes away.

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

                record.output(&prepared.label, &line, stderr);

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
                        stop(pid, &mut child, &mut requests).await;
                    }
                }
            }
            () = &mut timed_out, if stopped.is_none() => {
                let timeout = prepared.timeout.unwrap_or_default();
                stopped = Some(format!(
                    "it ran past its timeout of {}",
                    muru_config::format_duration(timeout)
                ));
                stop(pid, &mut child, &mut requests).await;
            }
            () = sender.closed(), if !caller_gone => {
                caller_gone = true;

                if stopped.is_none() {
                    stopped = Some("its caller went away".to_owned());
                    stop(pid, &mut child, &mut requests).await;
                }
            }
            request = next_request(&mut requests) => match request {
                Request::Stop(why) => {
                    if stopped.is_none() {
                        stopped = Some(why);
                        stop(pid, &mut child, &mut requests).await;
                    }
                }
                Request::Kill => {
                    stopped.get_or_insert_with(|| "its caller had it killed".to_owned());
                    kill(pid, &mut child);
                }
            },
        }
    }

    let took = started.elapsed();
    let ending = ending(child.wait().await, stopped, took);

    if let Some(log) = log.as_mut() {
        let _ = writeln!(log, "[command {}] {}", prepared.label, ending.message);
    }

    record.output(&prepared.label, &ending.message, false);

    let (kind, level) = if ending.ok {
        ("command.finished", "info")
    } else {
        ("command.failed", "warning")
    };
    record.event(
        kind,
        &subject,
        level,
        &format!("{}: {}", prepared.label, ending.message),
    );

    if let Some(name) = &prepared.declared {
        let run = CommandRun {
            release: prepared.release.clone(),
            started_unix_ms,
            duration_ms: took.as_millis() as u64,
            exit_code: ending.code,
            ok: ending.ok,
            message: ending.message.clone(),
        };

        record.ended(&prepared.app, name, &run);
    }

    if !caller_gone {
        let _ = sender.send(finished(&prepared, &ending)).await;
    }
}

/// Stop the command's session: SIGTERM, a grace period, then SIGKILL,
/// sooner when the caller asks for the command to be killed.
async fn stop(
    pid: u32,
    child: &mut tokio::process::Child,
    requests: &mut mpsc::UnboundedReceiver<Request>,
) {
    if pid == 0 {
        let _ = child.start_kill();
        return;
    }

    crate::host::signal_group(pid, libc::SIGTERM);

    let ended = tokio::select! {
        waited = tokio::time::timeout(STOP_GRACE, child.wait()) => waited.is_ok(),
        () = kill_asked(requests) => false,
    };

    if !ended {
        kill(pid, child);
    }
}

/// Kill the command's session at once.
fn kill(pid: u32, child: &mut tokio::process::Child) {
    if pid == 0 {
        let _ = child.start_kill();
    } else {
        crate::host::signal_group(pid, libc::SIGKILL);
    }
}

/// What the caller asks next, which never comes once it has dropped its
/// stopper.
async fn next_request(requests: &mut mpsc::UnboundedReceiver<Request>) -> Request {
    match requests.recv().await {
        Some(request) => request,
        None => std::future::pending().await,
    }
}

/// Wait for the caller to ask for the command to be killed.
async fn kill_asked(requests: &mut mpsc::UnboundedReceiver<Request>) {
    while !matches!(next_request(requests).await, Request::Kill) {}
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
            log: Some(log),
            timeout: None,
            declared: None,
            exclusive: false,
            inputs: String::new(),
            secrets: None,
        }
    }

    fn recorded() -> Arc<dyn Record> {
        let store = store();
        Arc::new(Recorded {
            events: Events::new(store.clone()),
            store,
        })
    }

    fn store() -> Arc<Store> {
        Arc::new(Store::in_memory().expect("a store"))
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

    fn declared() -> CommandSpec {
        let input = |name: &str| crate::model::CommandInputSpec {
            name: name.to_owned(),
            description: String::new(),
            required: false,
            default: None,
            pattern: None,
            secret: false,
        };

        CommandSpec {
            name: "add-user".to_owned(),
            description: String::new(),
            run: vec!["add".to_owned(), "{username}".to_owned()],
            call: None,
            env: Vec::new(),
            inputs: vec![
                crate::model::CommandInputSpec {
                    required: true,
                    pattern: Some("[a-z]+".to_owned()),
                    ..input("username")
                },
                crate::model::CommandInputSpec {
                    default: Some("staff".to_owned()),
                    ..input("role")
                },
                crate::model::CommandInputSpec {
                    secret: true,
                    pattern: Some("\\S{8,}".to_owned()),
                    ..input("password")
                },
                input("note"),
            ],
            timeout_ms: None,
            exclusive: false,
        }
    }

    #[test]
    fn placeholders_name_inputs_and_doubled_braces_are_braces() {
        assert_eq!(
            placeholders("--user={username} {{literal}} {role}"),
            Ok(vec!["username".to_owned(), "role".to_owned()])
        );

        let values = BTreeMap::from([("username".to_owned(), "bob".to_owned())]);
        assert_eq!(
            substitute("--user={username} {{x}}", &values),
            Ok("--user=bob {x}".to_owned())
        );
        assert!(
            substitute("{missing}", &values)
                .unwrap_err()
                .contains("names no input")
        );
        assert!(placeholders("{unclosed").unwrap_err().contains("no }"));
        assert!(placeholders("stray }").unwrap_err().contains("no {"));
        assert!(
            placeholders("{not a name}")
                .unwrap_err()
                .contains("not an input name")
        );
    }

    #[test]
    fn inputs_are_checked_against_the_declaration() {
        let command = declared();
        let given = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>()
        };

        let values = input_values(&command, &given(&[("username", "bob")])).expect("valid");
        assert_eq!(values["role"], "staff");
        assert_eq!(values["note"], "");

        assert_eq!(
            input_values(&command, &given(&[])),
            Err("add-user needs the input username".to_owned())
        );
        assert!(
            input_values(&command, &given(&[("username", "Bob1")]))
                .unwrap_err()
                .contains("\"Bob1\" does not match")
        );
        assert!(
            input_values(&command, &given(&[("username", "bob"), ("colour", "red")]))
                .unwrap_err()
                .contains("no input named colour; it takes username, role, password, note")
        );

        // A secret's value is never repeated, in an error or an event.

        let short = input_values(
            &command,
            &given(&[("username", "bob"), ("password", "hunter2")]),
        )
        .unwrap_err();
        assert!(!short.contains("hunter2"), "{short}");

        let values = input_values(
            &command,
            &given(&[("username", "bob"), ("password", "correct horse")]),
        );
        assert!(values.is_err(), "a space is not \\S");

        let values = input_values(
            &command,
            &given(&[("username", "bob"), ("password", "correcthorse")]),
        )
        .expect("valid");
        assert_eq!(
            describe_inputs(&command, &values),
            "username=bob, role=staff, password=***, note="
        );
    }

    #[test]
    fn a_declared_command_gets_its_inputs_in_place() {
        let python = Path::new("/srv/env/bin/python");
        let base = || {
            let mut base = prepared(&[], PathBuf::from("/dev/null"));
            base.env = vec![("PASSWORD".to_owned(), "from-the-release".to_owned())];
            base
        };
        let mut command = declared();
        command.run = vec![
            "python".to_owned(),
            "add.py".to_owned(),
            "--user={username}".to_owned(),
        ];
        command.env = vec![("PASSWORD".to_owned(), "{password}".to_owned())];
        command.timeout_ms = Some(5_000);
        let given = BTreeMap::from([
            ("username".to_owned(), "bob".to_owned()),
            ("password".to_owned(), "correcthorse".to_owned()),
        ]);
        let values = input_values(&command, &given).expect("valid");

        let ready = super::declared(base(), &command, &given, &values, python).expect("ready");
        assert_eq!(ready.argv, ["/srv/env/bin/python", "add.py", "--user=bob"]);
        assert_eq!(ready.label, "add-user");
        assert_eq!(ready.declared.as_deref(), Some("add-user"));
        assert_eq!(ready.timeout, Some(Duration::from_secs(5)));
        assert!(
            ready
                .env
                .contains(&("PASSWORD".to_owned(), "correcthorse".to_owned()))
        );
        assert!(
            !ready
                .env
                .iter()
                .any(|(_, value)| value == "from-the-release")
        );
        assert!(
            ready
                .env
                .contains(&("MURU_COMMAND".to_owned(), "add-user".to_owned()))
        );
        assert!(ready.inputs.contains("password=***"), "{}", ready.inputs);

        // A callable gets the inputs given or defaulted, as JSON.

        command.run = Vec::new();
        command.call = Some("shop.ops:add".to_owned());
        let ready = super::declared(base(), &command, &given, &values, python).expect("ready");
        assert_eq!(
            ready.argv,
            ["/srv/env/bin/python", "-m", "muru._command", "shop.ops:add"]
        );
        let inputs = ready
            .env
            .iter()
            .find(|(name, _)| name == "MURU_COMMAND_INPUTS")
            .map(|(_, value)| {
                serde_json::from_str::<BTreeMap<String, String>>(value).expect("JSON")
            })
            .expect("the inputs");
        assert_eq!(
            inputs.keys().map(String::as_str).collect::<Vec<_>>(),
            ["password", "role", "username"]
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
        let receiver = run(
            prepared(
                &["sh", "-c", "echo $GREETING; echo oops >&2; exit 2"],
                log.clone(),
            ),
            recorded(),
        )
        .expect("started")
        .output;
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
        let mut command = prepared(&["sleep", "30"], dir.path().join("r1.log"));
        command.timeout = Some(Duration::from_millis(200));
        let messages = collect(run(command, recorded()).expect("started").output).await;

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
    async fn a_command_stopped_by_its_caller_ends_saying_why() {
        let dir = tempfile::tempdir().expect("a directory");
        let mut running = run(
            prepared(
                &["sh", "-c", "echo started; exec sleep 30"],
                dir.path().join("r1.log"),
            ),
            recorded(),
        )
        .expect("started");

        // The stop comes once the command is running, and the output is
        // read on to its end.

        let first = running.output.recv().await.expect("a line");
        assert_eq!(first.line, "started");
        running.stopper.stop("muructl got SIGTERM");
        let messages = collect(running.output).await;

        let last = messages.last().expect("a last message");
        assert!(last.finished);
        assert_eq!(last.exit_code, 128 + libc::SIGTERM);
        assert!(
            last.message
                .starts_with("stopped because muructl got SIGTERM; it was killed by signal 15"),
            "{}",
            last.message
        );
    }

    #[tokio::test]
    async fn a_command_that_ignores_sigterm_is_killed_when_its_caller_asks() {
        let dir = tempfile::tempdir().expect("a directory");
        let mut running = run(
            prepared(
                &[
                    "sh",
                    "-c",
                    "trap '' TERM; echo started; while :; do sleep 1; done",
                ],
                dir.path().join("r1.log"),
            ),
            recorded(),
        )
        .expect("started");

        // Asked to stop, it would hold out for the grace period; the kill
        // that follows ends it well inside that.

        let first = running.output.recv().await.expect("a line");
        assert_eq!(first.line, "started");
        running.stopper.stop("muructl got SIGINT");
        tokio::time::sleep(Duration::from_millis(100)).await;
        running.stopper.kill();
        let messages = tokio::time::timeout(STOP_GRACE / 2, collect(running.output))
            .await
            .expect("the command ended before the grace period did");

        let last = messages.last().expect("a last message");
        assert_eq!(last.exit_code, 128 + libc::SIGKILL);
        assert!(
            last.message
                .starts_with("stopped because muructl got SIGINT; it was killed by signal 9"),
            "{}",
            last.message
        );
    }

    #[tokio::test]
    async fn a_dropped_stopper_leaves_the_command_running() {
        let dir = tempfile::tempdir().expect("a directory");
        let Running { output, stopper } = run(
            prepared(
                &["sh", "-c", "sleep 0.2; echo done"],
                dir.path().join("r1.log"),
            ),
            recorded(),
        )
        .expect("started");
        drop(stopper);
        let messages = collect(output).await;

        let last = messages.last().expect("a last message");
        assert_eq!(last.exit_code, 0, "{}", last.message);
        assert!(messages.iter().any(|message| message.line == "done"));
    }

    #[tokio::test]
    async fn a_program_that_is_not_found_ends_with_127() {
        let dir = tempfile::tempdir().expect("a directory");
        let messages = collect(
            run(
                prepared(&["no-such-program-anywhere"], dir.path().join("r1.log")),
                recorded(),
            )
            .expect("started")
            .output,
        )
        .await;

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].exit_code, 127);
        assert!(messages[0].message.contains("could not be started"));
    }

    /// A record that keeps what it is given, as a development run's does.
    #[derive(Default)]
    struct Kept {
        output: Mutex<Vec<String>>,
        events: Mutex<Vec<String>>,
        ended: Mutex<Vec<(String, i32)>>,
    }

    impl Record for Kept {
        fn output(&self, label: &str, line: &str, _stderr: bool) {
            self.output
                .lock()
                .expect("output")
                .push(format!("[command {label}] {line}"));
        }

        fn event(&self, kind: &str, subject: &str, _level: &'static str, _message: &str) {
            self.events
                .lock()
                .expect("events")
                .push(format!("{kind} {subject}"));
        }

        fn ended(&self, _app: &str, name: &str, run: &CommandRun) {
            self.ended
                .lock()
                .expect("ended")
                .push((name.to_owned(), run.exit_code));
        }
    }

    #[tokio::test]
    async fn a_run_with_no_log_is_recorded_by_whoever_started_it() {
        let dir = tempfile::tempdir().expect("a directory");
        let kept = Arc::new(Kept::default());
        let mut command = prepared(&["sh", "-c", "echo hello; exit 3"], dir.path().join("x"));
        command.log = None;
        command.label = "migrate".to_owned();
        command.declared = Some("migrate".to_owned());
        let messages = collect(run(command, kept.clone()).expect("started").output).await;

        assert_eq!(messages.last().expect("a last message").exit_code, 3);
        assert!(!dir.path().join("x").exists());

        let output = kept.output.lock().expect("output").clone();
        assert_eq!(output[0], "[command migrate] hello");
        assert!(
            output[1].starts_with("[command migrate] exited with status 3 after"),
            "{output:?}"
        );
        assert_eq!(
            *kept.events.lock().expect("events"),
            ["command.started shop@r1", "command.failed shop@r1"]
        );
        assert_eq!(
            *kept.ended.lock().expect("ended"),
            [("migrate".to_owned(), 3)]
        );
    }
}
