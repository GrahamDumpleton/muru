//! `muructl serve` (section 13.9 of the specification, ADR 0044): one
//! application as one release in a container, configured from the image
//! and the environment, with nothing kept between starts.
//!
//! The command lays its settings in layers, the project's `[tool.muru]`
//! table, its serve table, the file `MURU_SERVE_CONFIG` names, one
//! environment variable per flag and the flags, and then serves through
//! the same code as `muructl run`, with the production behaviours on and
//! the container's own: one run directory, no installs, the admin port
//! and one output stream.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Args, Parser};
use muru_controller::cgroup::Limits;

/// Where everything `serve` writes goes, unless told otherwise.
pub const DEFAULT_RUN_DIR: &str = "/run/muru";
/// Where `muructl build --runtime-dir` puts the runtime library in an
/// image, unless told otherwise.
pub const DEFAULT_RUNTIME_DIR: &str = "/opt/muru";
/// The application's address by default.
const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
/// The admin port's address by default.
const DEFAULT_ADMIN_LISTEN: &str = "0.0.0.0:9090";
/// The spool's quota by default: 1 GiB.
const DEFAULT_SPOOL_QUOTA: u64 = 1024 * 1024 * 1024;
/// The prefix of the environment variable for each flag.
const ENV_PREFIX: &str = "MURU_SERVE_";
/// The probes' socket in the run directory.
pub const PROBE_SOCKET: &str = "probe.sock";
/// The admin socket in the run directory.
pub const ADMIN_SOCKET: &str = "admin.sock";

/// The arguments of `muructl serve`. Every setting can also come from the
/// project's serve table, the file `--config` names, or an environment
/// variable named `MURU_SERVE_` and the flag in upper snake case.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// The application as module:callable, or the project directory whose
    /// [tool.muru] table names it (default: the working directory).
    #[arg(value_name = "ENTRYPOINT or PATH")]
    app: Option<String>,

    /// The directory to find the project in.
    #[arg(long, value_name = "DIR")]
    chdir: Option<PathBuf>,

    /// A settings file holding the serve table's keys at its top level
    /// (also MURU_SERVE_CONFIG).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Where the application is served (default 0.0.0.0:8080).
    #[arg(long, value_name = "ADDRESS")]
    listen: Option<String>,

    /// Where the probes and metrics are served, or off (default
    /// 0.0.0.0:9090).
    #[arg(long, value_name = "ADDRESS")]
    admin_listen: Option<String>,

    /// Serve /status on the admin port too: the release, its workers and
    /// the settings worked out, as JSON.
    #[arg(long)]
    admin_status: bool,

    /// Workers ready in every group for /readyz to pass (default 1).
    #[arg(long, value_name = "N")]
    min_ready: Option<String>,

    /// The interface the entrypoint speaks: auto, wsgi, asgi or grpc.
    #[arg(long, value_name = "NAME")]
    interface: Option<String>,

    /// The entrypoint is a factory, called in each worker.
    #[arg(long)]
    factory: bool,

    /// Worker processes, or auto for the CPU count (the default).
    #[arg(long, value_name = "N")]
    processes: Option<String>,

    /// Application threads per worker (default 4).
    #[arg(long, value_name = "N")]
    threads: Option<String>,

    /// Threads for synchronous code called from an ASGI application
    /// (default 8).
    #[arg(long, value_name = "N")]
    sync_threads: Option<String>,

    /// Requests an ASGI worker's event loop takes at once (default 1000).
    #[arg(long, value_name = "N")]
    concurrency: Option<String>,

    /// Import the application before forking: auto, true or false.
    #[arg(long, value_name = "MODE")]
    preload: Option<String>,

    /// Module providing the lifecycle hooks.
    #[arg(long, value_name = "MODULE")]
    hooks: Option<String>,

    /// Override a timeout as NAME=DURATION, as muructl run takes it.
    /// Repeatable.
    #[arg(long = "timeout", value_name = "NAME=DURATION")]
    timeout: Vec<String>,

    /// Largest request body accepted (default 10MB).
    #[arg(long, value_name = "SIZE")]
    max_body_size: Option<String>,

    /// Replace a worker after this many requests, with jitter.
    #[arg(long, value_name = "N")]
    max_requests: Option<String>,

    /// Replace a worker after this long, with jitter.
    #[arg(long, value_name = "DURATION")]
    max_lifetime: Option<String>,

    /// A memory setting as KEY=VALUE, as muructl run takes it.
    /// Repeatable.
    #[arg(long = "memory", value_name = "KEY=VALUE")]
    memory: Vec<String>,

    /// Open connections the listener holds at once.
    #[arg(long, value_name = "N")]
    max_connections: Option<String>,

    /// Open connections one client address may hold.
    #[arg(long, value_name = "N")]
    max_connections_per_client: Option<String>,

    /// Requests served on one connection before it is closed.
    #[arg(long, value_name = "N")]
    max_requests_per_connection: Option<String>,

    /// A directory of files the edge serves, as PREFIX=DIR. Repeatable.
    #[arg(long = "static", value_name = "PREFIX=DIR")]
    static_dirs: Vec<String>,

    /// Do not compress responses.
    #[arg(long)]
    no_compression: bool,

    /// The access log: on (the default), errors or off.
    #[arg(long, value_name = "WHAT")]
    access_log: Option<String>,

    /// Leave requests whose path matches the pattern out of the access
    /// log. Repeatable.
    #[arg(long = "access-log-exclude", value_name = "PATTERN")]
    access_log_exclude: Vec<String>,

    /// Requests at least this slow count as errors for --access-log
    /// errors (default 1s).
    #[arg(long, value_name = "DURATION")]
    slow: Option<String>,

    /// The lines on standard output: text (the default), or json for a
    /// log collector.
    #[arg(long, value_name = "FORMAT")]
    log_format: Option<String>,

    /// Put the full UTC time in front of every line.
    #[arg(long)]
    timestamps: bool,

    /// The interpreter (default: VIRTUAL_ENV, the project's .venv,
    /// /app/.venv, python3 on PATH).
    #[arg(long, value_name = "PATH")]
    python: Option<String>,

    /// Where everything written at run time goes (default /run/muru, or
    /// MURU_RUN_DIR).
    #[arg(long, value_name = "DIR")]
    run_dir: Option<String>,

    /// Where the image holds the runtime library (default /opt/muru).
    #[arg(long, value_name = "DIR")]
    runtime_dir: Option<String>,

    /// The most request bodies spooled to disk at once (default 1GB).
    #[arg(long, value_name = "SIZE")]
    spool_quota: Option<String>,

    /// A proxy whose forwarding headers are applied: an address, a CIDR
    /// range, or loopback, private or unix (default loopback).
    /// Repeatable.
    #[arg(long = "trusted-proxy", value_name = "ADDRESS")]
    trusted_proxy: Vec<String>,

    /// How long the listener keeps accepting after SIGTERM, with
    /// readiness failing and every connection closing after its response
    /// (default 0s; 5s behind a Kubernetes Service).
    #[arg(long, value_name = "DURATION")]
    shutdown_delay: Option<String>,

    /// How long the whole stop may take, the delay included; what has
    /// not finished is killed (default 8s).
    #[arg(long, value_name = "DURATION")]
    shutdown_budget: Option<String>,

    /// Print the settings that would be used, each with where it came
    /// from, and exit.
    #[arg(long)]
    print_config: bool,
}

/// How a setting is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// One value.
    Text,
    /// A list, comma-separated in an environment variable.
    List,
    /// On or off.
    Flag,
}

/// Every setting, by its key: the flag in snake case.
const KEYS: &[(&str, Kind)] = &[
    ("entrypoint", Kind::Text),
    ("listen", Kind::Text),
    ("admin_listen", Kind::Text),
    ("admin_status", Kind::Flag),
    ("min_ready", Kind::Text),
    ("interface", Kind::Text),
    ("factory", Kind::Flag),
    ("processes", Kind::Text),
    ("threads", Kind::Text),
    ("sync_threads", Kind::Text),
    ("concurrency", Kind::Text),
    ("preload", Kind::Text),
    ("hooks", Kind::Text),
    ("timeout", Kind::List),
    ("max_body_size", Kind::Text),
    ("max_requests", Kind::Text),
    ("max_lifetime", Kind::Text),
    ("memory", Kind::List),
    ("max_connections", Kind::Text),
    ("max_connections_per_client", Kind::Text),
    ("max_requests_per_connection", Kind::Text),
    ("static", Kind::List),
    ("no_compression", Kind::Flag),
    ("access_log", Kind::Text),
    ("access_log_exclude", Kind::List),
    ("slow", Kind::Text),
    ("log_format", Kind::Text),
    ("timestamps", Kind::Flag),
    ("python", Kind::Text),
    ("run_dir", Kind::Text),
    ("runtime_dir", Kind::Text),
    ("spool_quota", Kind::Text),
    ("trusted_proxy", Kind::List),
    ("shutdown_delay", Kind::Text),
    ("shutdown_budget", Kind::Text),
];

/// A setting's value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Text(String),
    List(Vec<String>),
    Flag(bool),
}

impl std::fmt::Display for Value {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => write!(formatter, "{text:?}"),
            Self::List(items) => write!(formatter, "{items:?}"),
            Self::Flag(on) => write!(formatter, "{on}"),
        }
    }
}

/// The settings laid in layers, each with where it came from.
#[derive(Debug, Default)]
struct Layered {
    values: BTreeMap<&'static str, (Value, String)>,
}

impl Layered {
    fn text(&self, key: &str) -> Option<&str> {
        match self.values.get(key) {
            Some((Value::Text(text), _)) => Some(text),
            _ => None,
        }
    }

    fn list(&self, key: &str) -> &[String] {
        match self.values.get(key) {
            Some((Value::List(items), _)) => items,
            _ => &[],
        }
    }

    fn flag(&self, key: &str) -> bool {
        matches!(self.values.get(key), Some((Value::Flag(true), _)))
    }

    /// Lay `layer` over what is there.
    fn over(&mut self, layer: BTreeMap<&'static str, Value>, source: &str) {
        for (key, value) in layer {
            self.values.insert(key, (value, source.to_owned()));
        }
    }
}

/// The flags given, as settings.
fn from_flags(args: &ServeArgs) -> BTreeMap<&'static str, Value> {
    let mut layer = BTreeMap::new();
    let mut text = |key: &'static str, value: &Option<String>| {
        if let Some(value) = value {
            layer.insert(key, Value::Text(value.clone()));
        }
    };

    text("entrypoint", &args.app);
    text("listen", &args.listen);
    text("admin_listen", &args.admin_listen);
    text("min_ready", &args.min_ready);
    text("interface", &args.interface);
    text("processes", &args.processes);
    text("threads", &args.threads);
    text("sync_threads", &args.sync_threads);
    text("concurrency", &args.concurrency);
    text("preload", &args.preload);
    text("hooks", &args.hooks);
    text("max_body_size", &args.max_body_size);
    text("max_requests", &args.max_requests);
    text("max_lifetime", &args.max_lifetime);
    text("max_connections", &args.max_connections);
    text(
        "max_connections_per_client",
        &args.max_connections_per_client,
    );
    text(
        "max_requests_per_connection",
        &args.max_requests_per_connection,
    );
    text("access_log", &args.access_log);
    text("slow", &args.slow);
    text("log_format", &args.log_format);
    text("python", &args.python);
    text("run_dir", &args.run_dir);
    text("runtime_dir", &args.runtime_dir);
    text("spool_quota", &args.spool_quota);
    text("shutdown_delay", &args.shutdown_delay);
    text("shutdown_budget", &args.shutdown_budget);

    for (key, items) in [
        ("timeout", &args.timeout),
        ("memory", &args.memory),
        ("static", &args.static_dirs),
        ("access_log_exclude", &args.access_log_exclude),
        ("trusted_proxy", &args.trusted_proxy),
    ] {
        if !items.is_empty() {
            layer.insert(key, Value::List(items.clone()));
        }
    }

    for (key, on) in [
        ("admin_status", args.admin_status),
        ("factory", args.factory),
        ("no_compression", args.no_compression),
        ("timestamps", args.timestamps),
    ] {
        if on {
            layer.insert(key, Value::Flag(true));
        }
    }

    layer
}

/// The environment's settings: `MURU_SERVE_` and the key in upper case,
/// a list separated by commas, a flag true or false.
fn from_environment(
    read: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<BTreeMap<&'static str, Value>> {
    let mut layer = BTreeMap::new();

    for (key, kind) in KEYS {
        let name = format!("{ENV_PREFIX}{}", key.to_ascii_uppercase());

        let Some(raw) = read(&name) else {
            continue;
        };

        let value = match kind {
            Kind::Text => Value::Text(raw),
            Kind::List => Value::List(
                raw.split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_owned)
                    .collect(),
            ),
            Kind::Flag => Value::Flag(
                parse_flag(&raw).with_context(|| format!("{name}={raw:?} is not true or false"))?,
            ),
        };

        layer.insert(*key, value);
    }

    Ok(layer)
}

fn parse_flag(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

/// A serve table or settings file, as settings.
fn from_table(table: &muru_config::ServeSettings) -> BTreeMap<&'static str, Value> {
    let mut layer = BTreeMap::new();
    let Ok(serde_json::Value::Object(object)) = serde_json::to_value(table) else {
        return layer;
    };

    for (name, value) in object {
        // The table says compression = false where the flag says
        // --no-compression.

        if name == "compression" {
            layer.insert("no_compression", Value::Flag(value == false));
            continue;
        }

        let Some((key, _)) = KEYS.iter().find(|(key, _)| *key == name) else {
            continue;
        };

        let value = match value {
            serde_json::Value::String(text) => Value::Text(text),
            serde_json::Value::Number(number) => Value::Text(number.to_string()),
            serde_json::Value::Bool(on) => Value::Flag(on),
            serde_json::Value::Array(items) => Value::List(
                items
                    .into_iter()
                    .map(|item| match item {
                        serde_json::Value::String(text) => text,
                        other => other.to_string(),
                    })
                    .collect(),
            ),
            _ => continue,
        };

        layer.insert(*key, value);
    }

    layer
}

/// What makes a run `muructl serve`'s, passed to the shared serving code.
#[derive(Debug, Clone)]
pub struct Mode {
    /// Where the probes and metrics are served, or nowhere.
    pub admin_listen: Option<SocketAddr>,
    /// Whether `/status` is served.
    pub admin_status: bool,
    /// Workers ready in every group for `/readyz`.
    pub min_ready: usize,
    /// Where everything written at run time goes.
    pub run_dir: PathBuf,
    /// Where the image holds the runtime library.
    pub runtime_dir: PathBuf,
    /// Lines as JSON objects.
    pub json: bool,
    /// The spool's quota in bytes.
    pub spool_quota: u64,
    /// The worker count when nothing sets one.
    pub default_processes: usize,
    /// Whose forwarding headers the edge applies.
    pub trust: muru_edge::proxy::Trust,
    /// The delay and budget of the stop SIGTERM starts.
    pub shutdown: muru_controller::shutdown::Policy,
    /// The container's memory budget, for the workers' hard limits.
    pub memory_budget: Option<u64>,
    /// The container's cgroup directory.
    pub cgroup: Option<PathBuf>,
    /// What was worked out from the container's limits, a line each.
    pub sizing: Vec<String>,
}

/// A clap parser for `muructl run`'s arguments alone, which `serve` fills
/// from its settings.
#[derive(Parser)]
struct RunOnly {
    #[command(flatten)]
    args: crate::RunArgs,
}

/// Run `muructl serve`.
pub async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    // Section 13.9: every line goes to standard output, a failure to
    // start included, in the format asked for.

    muru_common::log::one_stream(true);

    let limits = Limits::read();

    let result = match prepare(&args, limits.as_ref(), |name| std::env::var(name).ok()) {
        Ok((_, mode, layered)) if args.print_config => {
            print_config(&layered, &mode);
            return Ok(());
        }
        Ok((run_args, mode, _)) => {
            muru_common::log::json_lines(mode.json);

            for line in &mode.sizing {
                muru_common::muru_println!("{line}");
            }

            crate::run_with(run_args, Some(mode)).await
        }
        Err(error) => Err(error),
    };

    if let Err(error) = &result {
        muru_common::muru_println!("error: {error:#}");
        std::process::exit(1);
    }

    result
}

/// Lay the settings, check them, and turn them into `muructl run`'s
/// arguments with what is `serve`'s own beside them.
fn prepare(
    args: &ServeArgs,
    limits: Option<&Limits>,
    read: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<(crate::RunArgs, Mode, Layered)> {
    // The project is found before its tables are read, from the argument
    // or --chdir as muructl run finds it.

    let app = args.app.clone().or_else(|| read("MURU_SERVE_ENTRYPOINT"));
    let chdir = args
        .chdir
        .clone()
        .or_else(|| read("MURU_SERVE_CHDIR").map(PathBuf::from));

    let project = match app.as_deref() {
        Some(app) if Path::new(app).is_dir() => PathBuf::from(app),
        _ => chdir.clone().unwrap_or_else(|| PathBuf::from(".")),
    };

    let mut layered = Layered::default();
    let table = muru_config::load_project(&project)?;

    if let Some((path, tool)) = &table
        && let Some(serve) = &tool.serve
    {
        layered.over(from_table(serve), &format!("[serve] in {}", path.display()));
    }

    let config = args
        .config
        .clone()
        .or_else(|| read("MURU_SERVE_CONFIG").map(PathBuf::from));

    if let Some(path) = &config {
        let file = muru_config::load_serve_file(path).map_err(|error| anyhow::anyhow!(error))?;
        layered.over(from_table(&file), &path.display().to_string());
    }

    let mut environment = from_environment(&read)?;

    if !environment.contains_key("run_dir")
        && let Some(dir) = read("MURU_RUN_DIR")
    {
        environment.insert("run_dir", Value::Text(dir));
    }

    layered.over(environment, "environment");
    layered.over(from_flags(args), "command line");

    // An entrypoint from a layer is the argument, unless the argument is
    // the project directory.

    let entrypoint = match app.as_deref() {
        Some(app) if Path::new(app).is_dir() => None,
        _ => layered.text("entrypoint").map(str::to_owned),
    };

    let listen: SocketAddr = layered
        .text("listen")
        .unwrap_or(DEFAULT_LISTEN)
        .parse()
        .with_context(|| "--listen takes IP:PORT")?;

    let admin_listen = match layered.text("admin_listen").unwrap_or(DEFAULT_ADMIN_LISTEN) {
        "off" => None,
        address => Some(
            address
                .parse::<SocketAddr>()
                .with_context(|| "--admin-listen takes IP:PORT or off")?,
        ),
    };

    let min_ready = match layered.text("min_ready") {
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|count| *count >= 1)
            .with_context(|| format!("--min-ready {text:?} is not a number of at least 1"))?,
        None => 1,
    };

    let json = match layered.text("log_format").unwrap_or("text") {
        "text" => false,
        "json" => true,
        other => anyhow::bail!("--log-format {other:?} is not text or json"),
    };

    let spool_quota = match layered.text("spool_quota") {
        Some(text) => muru_config::parse_size(text)
            .map_err(|error| anyhow::anyhow!("--spool-quota {error}"))?,
        None => DEFAULT_SPOOL_QUOTA,
    };

    // Section 13.9: the worker count and the memory budget from the
    // container's limits, where cgroup v2 gives them, with a line for
    // each saying what it came from.

    let (default_processes, cpu_source) = match limits.and_then(Limits::processes) {
        Some((count, source)) => (count, Some(source)),
        None => (
            std::thread::available_parallelism().map_or(1, usize::from),
            None,
        ),
    };

    let processes_set = layered.text("processes").is_some_and(|text| text != "auto")
        || table
            .as_ref()
            .is_some_and(|(_, tool)| tool.processes.is_some());
    let memory_budget = limits.and_then(Limits::memory_budget);
    let mut sizing = Vec::new();

    if let Some(source) = cpu_source
        && !processes_set
    {
        sizing.push(format!("{default_processes} processes from {source}"));
    }

    if let (Some((budget, headroom)), Some(limit)) =
        (memory_budget, limits.and_then(|limits| limits.memory_max))
    {
        sizing.push(format!(
            "memory budget {} (memory.max {} less {})",
            muru_config::format_size_approx(budget),
            muru_config::format_size_approx(limit),
            muru_config::format_size_approx(headroom)
        ));
    }

    let trust = match layered.list("trusted_proxy") {
        [] => muru_edge::proxy::Trust::loopback(),
        trusted => {
            let table = muru_config::Proxy {
                trusted: trusted.to_vec(),
                ..Default::default()
            };
            muru_edge::proxy::Trust::from_snapshot(&muru_controller::daemon::proxy_message(&table))
                .map_err(|error| anyhow::anyhow!("--trusted-proxy {error}"))?
        }
    };

    // Section 13.3: the daemon's [shutdown] table, checked the same way,
    // always shutting down since serve is the container's main process.

    let shutdown = muru_controller::shutdown::Policy::from_table(
        Some(&muru_config::Shutdown {
            on_sigterm: Some("shutdown".to_owned()),
            delay: layered.text("shutdown_delay").map(str::to_owned),
            budget: layered.text("shutdown_budget").map(str::to_owned),
        }),
        true,
    )
    .map_err(|error| anyhow::anyhow!("shutdown_{error}"))?;

    let mode = Mode {
        admin_listen,
        admin_status: layered.flag("admin_status"),
        min_ready,
        run_dir: PathBuf::from(layered.text("run_dir").unwrap_or(DEFAULT_RUN_DIR)),
        runtime_dir: PathBuf::from(layered.text("runtime_dir").unwrap_or(DEFAULT_RUNTIME_DIR)),
        json,
        spool_quota,
        default_processes,
        trust,
        shutdown,
        memory_budget: memory_budget.map(|(budget, _)| budget),
        cgroup: limits.map(|limits| limits.dir.clone()),
        sizing,
    };

    // muructl run's arguments: production behaviours on, nothing built,
    // no dashboard, no reload.

    let mut argv: Vec<String> = vec!["muructl".to_owned()];

    match (&app, &entrypoint) {
        (Some(app), _) if Path::new(app).is_dir() => argv.push(app.clone()),
        (_, Some(entrypoint)) => argv.push(entrypoint.clone()),
        _ => {}
    }

    if let Some(chdir) = &chdir
        && !app.as_deref().is_some_and(|app| Path::new(app).is_dir())
    {
        argv.extend(["--chdir".to_owned(), chdir.display().to_string()]);
    }

    argv.extend(
        [
            "--allow-public",
            "--no-reload",
            "--build",
            "never",
            "--dashboard",
            "off",
            "--bind",
        ]
        .map(str::to_owned),
    );
    argv.push(listen.to_string());

    for (key, flag) in [
        ("interface", "--interface"),
        ("threads", "--threads"),
        ("sync_threads", "--sync-threads"),
        ("concurrency", "--concurrency"),
        ("preload", "--preload"),
        ("hooks", "--hooks"),
        ("max_body_size", "--max-body-size"),
        ("max_requests", "--max-requests"),
        ("max_lifetime", "--max-lifetime"),
        ("max_connections", "--max-connections"),
        ("max_connections_per_client", "--max-connections-per-client"),
        (
            "max_requests_per_connection",
            "--max-requests-per-connection",
        ),
        ("slow", "--slow"),
    ] {
        if let Some(value) = layered.text(key) {
            argv.extend([flag.to_owned(), value.to_owned()]);
        }
    }

    match layered.text("processes") {
        Some("auto") => argv.extend(["--processes".to_owned(), default_processes.to_string()]),
        Some(count) => argv.extend(["--processes".to_owned(), count.to_owned()]),
        None => {}
    }

    match layered.text("access_log").unwrap_or("on") {
        "on" => argv.extend(["--access-log".to_owned(), "terminal".to_owned()]),
        "errors" | "off" => {
            let what = layered.text("access_log").unwrap_or_default().to_owned();
            argv.extend(["--access-log".to_owned(), what]);
        }
        other => anyhow::bail!("--access-log {other:?} is not on, errors or off"),
    }

    for (key, flag) in [
        ("timeout", "--timeout"),
        ("memory", "--memory"),
        ("static", "--static"),
        ("access_log_exclude", "--access-log-exclude"),
    ] {
        for item in layered.list(key) {
            argv.extend([flag.to_owned(), item.clone()]);
        }
    }

    for (key, flag) in [
        ("factory", "--factory"),
        ("no_compression", "--no-compression"),
        ("timestamps", "--timestamps"),
    ] {
        if layered.flag(key) {
            argv.push(flag.to_owned());
        }
    }

    // Section 13.9: the image contract's interpreter, /app/.venv among
    // the places muructl run looks, when nothing nearer names one.

    let python = layered.text("python").map(str::to_owned).or_else(|| {
        let app_venv = Path::new("/app/.venv/bin/python");
        let nearer = read("VIRTUAL_ENV").is_some() || project.join(".venv").is_dir();
        (!nearer && app_venv.exists()).then(|| app_venv.display().to_string())
    });

    if let Some(python) = python {
        argv.extend(["--python".to_owned(), python]);
    }

    let run_args = RunOnly::try_parse_from(&argv)
        .map(|parsed| parsed.args)
        .map_err(|error| anyhow::anyhow!("{}", error.to_string().trim()))?;
    Ok((run_args, mode, layered))
}

/// `--print-config`: every setting given, with where it came from, and
/// what `serve` works out.
fn print_config(layered: &Layered, mode: &Mode) {
    for (key, (value, source)) in &layered.values {
        println!("{key} = {value}    # {source}");
    }

    println!(
        "# admin port {}, run directory {}, runtime library {}, {} lines, spool quota {} bytes, \
         {} processes unless set, {}",
        mode.admin_listen
            .map_or_else(|| "off".to_owned(), |address| address.to_string()),
        mode.run_dir.display(),
        mode.runtime_dir.display(),
        if mode.json { "json" } else { "text" },
        mode.spool_quota,
        mode.default_processes,
        mode.shutdown.describe()
    );

    for line in &mode.sizing {
        println!("# {line}");
    }
}

/// Make the run directory, or say which volume it needs (section 13.9).
pub fn make_run_dir(dir: &Path) -> anyhow::Result<()> {
    let writable = std::fs::create_dir_all(dir).and_then(|()| {
        let probe = dir.join(format!(".muru-{}", std::process::id()));
        std::fs::write(&probe, b"")?;
        std::fs::remove_file(&probe)
    });

    writable.map_err(|error| {
        anyhow::anyhow!(
            "the run directory {} cannot be written ({error}); with a read-only root \
             filesystem give it a writable volume, a tmpfs or an emptyDir mounted at {}, or \
             name another with --run-dir",
            dir.display(),
            dir.display()
        )
    })?;
    group_writable(dir);
    Ok(())
}

/// Make a directory serve creates writable by its group, which a
/// platform that runs containers under an arbitrary user ID shares as
/// group 0.
pub fn group_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o770));
}

/// Section 13.9: the runtime library, from the environment when it
/// matches muructl's version, else from the image's runtime directory,
/// and never fetched.
pub fn runtime_library(
    python: &muru_provision::Resolved,
    runtime_dir: &Path,
) -> anyhow::Result<muru_provision::Provided> {
    let version = muru_provision::runtime::pep440(env!("CARGO_PKG_VERSION"));

    if let Some(found) = &python.probe.muru
        && found.version == version
    {
        return Ok(muru_provision::Provided {
            pythonpath: None,
            version,
            how: muru_provision::runtime::How::Environment,
            notes: Vec::new(),
        });
    }

    let package = runtime_dir.join("muru");
    let installed = std::fs::read_dir(runtime_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix("muru-")
                .and_then(|rest| rest.strip_suffix(".dist-info"))
                .map(str::to_owned)
        })
        .next();

    match installed {
        Some(found) if package.is_dir() && found == version => Ok(muru_provision::Provided {
            pythonpath: Some(runtime_dir.to_path_buf()),
            version,
            how: muru_provision::runtime::How::Directory,
            notes: Vec::new(),
        }),
        Some(found) if package.is_dir() => anyhow::bail!(
            "the runtime library in {} is {found} and muructl is {version}; build the image \
             with the same muructl that runs it",
            runtime_dir.display()
        ),
        _ => anyhow::bail!(
            "no runtime library for {} ({}): build the image with muructl build --runtime-dir \
             {}, or add muru {version} to the environment",
            python.python.display(),
            python.probe.tag(),
            runtime_dir.display()
        ),
    }
}

/// `muructl probe`: ask a `muructl serve` in this container one of the
/// probes' questions over the socket in its run directory.
#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// live, ready or startup.
    #[arg(value_name = "PROBE")]
    probe: String,

    /// The run directory of the serve to ask (default MURU_RUN_DIR, else
    /// /run/muru).
    #[arg(long, value_name = "DIR")]
    run_dir: Option<PathBuf>,
}

/// Run `muructl probe`, exiting 0 when the answer is yes and 1 otherwise.
pub fn probe(args: ProbeArgs) -> anyhow::Result<()> {
    let path = match args.probe.as_str() {
        "live" => "/livez",
        "ready" => "/readyz",
        "startup" => "/startupz",
        other => anyhow::bail!("{other:?} is not a probe: live, ready or startup"),
    };

    let run_dir = args
        .run_dir
        .or_else(|| std::env::var_os("MURU_RUN_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_RUN_DIR));
    let socket = run_dir.join(PROBE_SOCKET);

    let answer = (|| -> std::io::Result<String> {
        let mut stream = std::os::unix::net::UnixStream::connect(&socket)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        write!(stream, "GET {path} HTTP/1.0\r\nHost: probe\r\n\r\n")?;
        let mut text = String::new();
        stream.read_to_string(&mut text)?;
        Ok(text)
    })();

    let text = match answer {
        Ok(text) => text,
        Err(error) => {
            println!(
                "muru: no muructl serve answers at {} ({error})",
                socket.display()
            );
            std::process::exit(1);
        }
    };

    let ok = text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200");
    let body = text
        .split_once("\r\n\r\n")
        .map_or("", |(_, body)| body)
        .trim();
    println!("{body}");
    std::process::exit(if ok { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn parse(argv: &[&str]) -> ServeArgs {
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: ServeArgs,
        }

        Cli::try_parse_from(std::iter::once("serve").chain(argv.iter().copied()))
            .expect("parses")
            .args
    }

    #[test]
    fn flags_win_over_the_environment_and_the_environment_over_defaults() {
        let args = parse(&["app:application", "--threads", "8"]);
        let environment = |name: &str| match name {
            "MURU_SERVE_THREADS" => Some("2".to_owned()),
            "MURU_SERVE_LISTEN" => Some("0.0.0.0:9000".to_owned()),
            "MURU_SERVE_ACCESS_LOG_EXCLUDE" => Some("/healthz, /static/*".to_owned()),
            "MURU_SERVE_TIMESTAMPS" => Some("true".to_owned()),
            _ => None,
        };
        let (run, mode, layered) = prepare(&args, None, environment).expect("prepares");

        assert_eq!(
            layered.values["threads"],
            (Value::Text("8".into()), "command line".into())
        );
        assert_eq!(layered.values["listen"].1, "environment");
        assert_eq!(
            layered.list("access_log_exclude"),
            ["/healthz".to_owned(), "/static/*".to_owned()]
        );
        assert!(layered.flag("timestamps"));
        assert_eq!(run.threads, Some(8));
        assert!(run.allow_public);
        assert_eq!(
            mode.admin_listen,
            Some("0.0.0.0:9090".parse().expect("address"))
        );
        assert_eq!(mode.run_dir, PathBuf::from(DEFAULT_RUN_DIR));
    }

    #[test]
    fn bad_values_are_refused_naming_the_flag() {
        let args = parse(&["app:application", "--log-format", "xml"]);
        let Err(error) = prepare(&args, None, |_| None) else {
            panic!("--log-format xml was accepted");
        };
        assert!(error.to_string().contains("--log-format"), "{error}");

        let args = parse(&["app:application"]);
        let Err(error) = prepare(&args, None, |name| {
            (name == "MURU_SERVE_ADMIN_STATUS").then(|| "maybe".to_owned())
        }) else {
            panic!("MURU_SERVE_ADMIN_STATUS=maybe was accepted");
        };
        assert!(
            format!("{error:#}").contains("MURU_SERVE_ADMIN_STATUS"),
            "{error:#}"
        );
    }

    #[test]
    fn trusted_proxies_are_checked() {
        let args = parse(&["app:application", "--trusted-proxy", "private"]);
        assert!(prepare(&args, None, |_| None).is_ok());

        let args = parse(&["app:application", "--trusted-proxy", "not-an-address"]);
        let Err(error) = prepare(&args, None, |_| None) else {
            panic!("a bad --trusted-proxy was accepted");
        };
        assert!(error.to_string().contains("--trusted-proxy"), "{error}");
    }

    #[test]
    fn the_shutdown_delay_fits_inside_its_budget() {
        let args = parse(&["app:application"]);
        let (_, mode, _) = prepare(&args, None, |_| None).expect("prepares");
        assert_eq!(mode.shutdown.delay, Duration::ZERO);
        assert_eq!(mode.shutdown.budget, Duration::from_secs(8));

        let args = parse(&["app:application", "--shutdown-delay", "5s"]);
        let (_, mode, _) = prepare(&args, None, |name| {
            (name == "MURU_SERVE_SHUTDOWN_BUDGET").then(|| "25s".to_owned())
        })
        .expect("prepares");
        assert_eq!(mode.shutdown.delay, Duration::from_secs(5));
        assert_eq!(mode.shutdown.budget, Duration::from_secs(25));

        let args = parse(&["app:application", "--shutdown-delay", "10s"]);
        let Err(error) = prepare(&args, None, |_| None) else {
            panic!("a delay longer than the budget was accepted");
        };
        assert!(error.to_string().contains("shutdown_delay"), "{error}");
    }

    #[test]
    fn the_container_limits_size_the_workers_and_say_so() {
        let limits = Limits {
            dir: PathBuf::from("/sys/fs/cgroup"),
            cpu_max: Some((150_000, 100_000)),
            cpus: Some(64),
            memory_max: Some(4 << 30),
        };
        let args = parse(&["app:application"]);
        let (run, mode, _) = prepare(&args, Some(&limits), |_| None).expect("prepares");
        assert_eq!(mode.default_processes, 2);
        assert_eq!(run.processes, None);
        assert_eq!(mode.cgroup, Some(PathBuf::from("/sys/fs/cgroup")));
        assert_eq!(mode.memory_budget, Some((4 << 30) - (4 << 30) / 100 * 10));
        assert_eq!(
            mode.sizing,
            [
                "2 processes from cpu.max 150000 100000",
                "memory budget 3.6GB (memory.max 4.0GB less 410MB)"
            ]
        );

        // A worker count given says nothing of the CPU limit.

        let args = parse(&["app:application", "--processes", "3"]);
        let (_, mode, _) = prepare(&args, Some(&limits), |_| None).expect("prepares");
        assert_eq!(mode.sizing.len(), 1);
    }

    #[test]
    fn the_admin_port_can_be_closed() {
        let args = parse(&["app:application", "--admin-listen", "off"]);
        let (_, mode, _) = prepare(&args, None, |_| None).expect("prepares");
        assert_eq!(mode.admin_listen, None);
    }
}
