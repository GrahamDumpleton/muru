//! `muructl render` (design specification section 10.4): the release a
//! command line describes as the table that declares it, from the flags,
//! the source's own settings and the defaults alone. No daemon is asked
//! and no interpreter is run, so the table can be written where neither
//! is at hand.

use std::collections::BTreeMap;
use std::path::PathBuf;

use muru_config::{Release as ReleaseTable, ToolMuru};
use muru_proto::admin::v1 as pb;

use crate::config::{self, format_ms};
use crate::model::{MemoryPolicy, ReleaseSpec, WorkerTimeouts};

/// Where a rendered setting came from. A single value has one origin,
/// the flags before the source's settings; a list the flags add to can
/// have both. With neither, it is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Origin {
    /// The source's own settings give it.
    pub source: bool,
    /// The flags give it.
    pub flags: bool,
}

impl Origin {
    /// From the flags.
    pub const FLAGS: Self = Self {
        source: false,
        flags: true,
    };
    /// From the source's own settings.
    pub const SOURCE: Self = Self {
        source: true,
        flags: false,
    };
    /// The default.
    pub const DEFAULT: Self = Self {
        source: false,
        flags: false,
    };
}

/// A rendered release.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The table that declares it.
    pub table: ReleaseTable,
    /// Where each setting came from, by its key within the table:
    /// `processes`, `timeouts.soft_timeout`, `env.NAME`, and for a table
    /// taken whole from the source's settings its name, such as `groups`.
    pub origins: BTreeMap<String, Origin>,
    /// The settings nothing gives and that have no default to write.
    pub unset: Vec<&'static str>,
    /// The file the source's settings were read from, when it has one.
    pub file: Option<PathBuf>,
}

/// The table being written, with where each of its settings came from.
struct Writer {
    /// Whether the defaults are written too.
    full: bool,
    origins: BTreeMap<String, Origin>,
    unset: Vec<&'static str>,
}

impl Writer {
    /// A setting the flags, the source's settings or a default give, in
    /// that order; one that nothing gives is noted as unset.
    fn one<T>(
        &mut self,
        key: &'static str,
        flag: Option<T>,
        table: Option<T>,
        default: Option<T>,
    ) -> Option<T> {
        let defaulted = default.is_some();

        let (value, origin) = match (flag, table) {
            (Some(value), _) => (Some(value), Origin::FLAGS),
            (None, Some(value)) => (Some(value), Origin::SOURCE),
            (None, None) => (default.filter(|_| self.full), Origin::DEFAULT),
        };

        if value.is_some() {
            self.origins.insert(key.to_owned(), origin);
        } else if !defaulted {
            self.unset.push(key);
        }

        value
    }

    /// A setting with no default worth naming when it is absent.
    fn given<T>(&mut self, key: &str, flag: Option<T>, table: Option<T>) -> Option<T> {
        let (value, origin) = match (flag, table) {
            (Some(value), _) => (value, Origin::FLAGS),
            (None, Some(value)) => (value, Origin::SOURCE),
            (None, None) => return None,
        };

        self.origins.insert(key.to_owned(), origin);
        Some(value)
    }

    /// A table or a list the source's settings give whole.
    fn whole(&mut self, key: &str, present: bool) {
        if present {
            self.origins.insert(key.to_owned(), Origin::SOURCE);
        }
    }
}

/// A setting a request writes as text: none when empty, else parsed.
fn parsed<T: std::str::FromStr>(text: &str) -> Option<T> {
    if text.is_empty() {
        None
    } else {
        text.parse().ok()
    }
}

/// A list the request adds to the table's, each name once.
fn beside(table: &[String], request: &[String]) -> Vec<String> {
    let mut all = table.to_vec();

    for name in request {
        if !all.contains(name) {
            all.push(name.clone());
        }
    }

    all
}

/// Render the release a deploy request describes: the request checked
/// as a deploy checks it, then written as the table an apply of which
/// gives the release a fresh deploy of the request would. `source` is
/// the directory as the table names it, which need not be the path the
/// request reads it by. With `full` the defaults are written too.
///
/// The interpreter is written only when the request or the source's
/// settings name one: the path a resolution here would find is the path
/// on this machine, and the table is applied on the node.
pub fn render(request: &pb::DeployRequest, source: &str, full: bool) -> Result<Rendered, String> {
    let spec = config::resolve_spec(
        request,
        None,
        &config::Tables::default(),
        &crate::provision::Provisioning::off(),
    )?;

    let mut writer = Writer {
        full,
        origins: BTreeMap::new(),
        unset: Vec::new(),
    };

    if spec.http.is_some() {
        let table = managed(request, source, &spec, &mut writer)?;

        return Ok(Rendered {
            table,
            origins: writer.origins,
            unset: writer.unset,
            file: None,
        });
    }

    let (file, table) =
        match muru_config::load_project(&spec.source).map_err(|error| error.to_string())? {
            Some((file, table)) => (Some(file), table),
            None => (None, ToolMuru::default()),
        };

    let table = workers(request, source, &spec, table, &mut writer);

    Ok(Rendered {
        table,
        origins: writer.origins,
        unset: writer.unset,
        file,
    })
}

/// A release of native workers: each setting from the request, else the
/// source's table, else with `full` the default.
fn workers(
    request: &pb::DeployRequest,
    source: &str,
    spec: &ReleaseSpec,
    table: ToolMuru,
    writer: &mut Writer,
) -> ReleaseTable {
    let text = |value: &str| (!value.is_empty()).then(|| value.to_owned());
    let count = |value: Option<u32>| value.map(|value| value as usize);

    // The entrypoint from the request replaces the table's with its
    // factory settings, as a deploy has it.

    let own_entrypoint = !request.entrypoint.is_empty();
    let entrypoint = writer
        .given("entrypoint", text(&request.entrypoint), table.entrypoint)
        .unwrap_or_default();

    let (factory, factory_phase) = if request.factory == Some(true) {
        (
            writer.given("factory", Some(true), None),
            writer.given("factory_phase", parsed(&request.factory_phase), None),
        )
    } else if !own_entrypoint && table.factory == Some(true) {
        (
            writer.given("factory", None, Some(true)),
            writer.given("factory_phase", None, table.factory_phase),
        )
    } else {
        (None, None)
    };

    let python = writer.given("python", text(&request.python), table.python);

    if python.is_none() {
        writer.unset.push("python");
    }

    // The build table: the mode as one setting, the extras and groups
    // the request adds beside the table's.

    let settings = table.build.unwrap_or_default();
    let when = writer.one(
        "build.when",
        parsed(&request.build_when),
        settings.when,
        Some(spec.build.when),
    );
    let mut list = |key: &str, table: &[String], request: &[String]| {
        if !table.is_empty() || !request.is_empty() {
            writer.origins.insert(
                key.to_owned(),
                Origin {
                    source: !table.is_empty(),
                    flags: !request.is_empty(),
                },
            );
        }

        beside(table, request)
    };
    let extras = list("build.extras", &settings.extras, &request.build_extras);
    let groups = list("build.groups", &settings.groups, &request.build_groups);
    let build = muru_config::Build {
        when,
        extras,
        groups,
        source_builds: writer.given("build.source_builds", None, settings.source_builds),
        compile_bytecode: writer.given("build.compile_bytecode", None, settings.compile_bytecode),
    };

    // The release's own directories of files, the request's over the
    // table's by path prefix.

    writer.whole("static", !table.static_dirs.is_empty());
    let mut static_dirs = table.static_dirs;

    for entry in &request.static_dirs {
        let given = muru_config::StaticDir {
            path: entry.path.clone(),
            dir: entry.dir.clone(),
            ..Default::default()
        };
        let key = muru_edge::routing::group_prefix(&entry.path);

        match static_dirs
            .iter_mut()
            .find(|existing| muru_edge::routing::group_prefix(&existing.path) == key)
        {
            Some(existing) => *existing = given,
            None => static_dirs.push(given),
        }

        writer.origins.entry("static".to_owned()).or_default().flags = true;
    }

    // The host's timeouts, each on its own; the request's limits on the
    // requests it serves are the table's, which no flag carries.

    let defaults = WorkerTimeouts::default();
    let given = table.timeouts.unwrap_or_default();
    let timeouts = muru_config::Timeouts {
        soft_timeout: writer.one(
            "timeouts.soft_timeout",
            request.soft_timeout_ms.map(format_ms),
            given.soft_timeout,
            None,
        ),
        hard_timeout: writer.one(
            "timeouts.hard_timeout",
            request.hard_timeout_ms.map(format_ms),
            given.hard_timeout,
            None,
        ),
        hard_drain_grace: writer.one(
            "timeouts.hard_drain_grace",
            request.hard_drain_grace_ms.map(format_ms),
            given.hard_drain_grace,
            Some(format_ms(defaults.hard_drain_grace_ms)),
        ),
        shutdown_timeout: writer.one(
            "timeouts.shutdown_timeout",
            request.shutdown_timeout_ms.map(format_ms),
            given.shutdown_timeout,
            Some(format_ms(defaults.shutdown_timeout_ms)),
        ),
        websocket_drain_grace: writer.one(
            "timeouts.websocket_drain_grace",
            request.websocket_drain_grace_ms.map(format_ms),
            given.websocket_drain_grace,
            Some(format_ms(defaults.websocket_drain_grace_ms)),
        ),
        loop_stall_timeout: writer.one(
            "timeouts.loop_stall_timeout",
            request.loop_stall_timeout_ms.map(format_ms),
            given.loop_stall_timeout,
            Some(format_ms(defaults.loop_stall_timeout_ms)),
        ),
        gil_stall_timeout: writer.one(
            "timeouts.gil_stall_timeout",
            request.gil_stall_timeout_ms.map(format_ms),
            given.gil_stall_timeout,
            Some(format_ms(defaults.gil_stall_timeout_ms)),
        ),
        queue_timeout: writer.given("timeouts.queue_timeout", None, given.queue_timeout),
        request_deadline: writer.given("timeouts.request_deadline", None, given.request_deadline),
        body_idle_timeout: writer.given(
            "timeouts.body_idle_timeout",
            None,
            given.body_idle_timeout,
        ),
        response_idle_timeout: writer.given(
            "timeouts.response_idle_timeout",
            None,
            given.response_idle_timeout,
        ),
    };

    // The memory policy, the same way.

    let defaults = MemoryPolicy::default();
    let given = table.memory.unwrap_or_default();
    let memory = muru_config::Memory {
        measure: writer.one(
            "memory.measure",
            parsed(&request.memory_measure),
            given.measure,
            Some(defaults.measure),
        ),
        soft_limit: writer.one(
            "memory.soft_limit",
            request.memory_soft_limit.map(muru_config::format_size),
            given.soft_limit,
            None,
        ),
        soft_for: writer.one(
            "memory.soft_for",
            request.memory_soft_for_ms.map(format_ms),
            given.soft_for,
            Some(format_ms(defaults.soft_for_ms)),
        ),
        reclaim_first: writer.one(
            "memory.reclaim_first",
            request.memory_reclaim_first,
            given.reclaim_first,
            Some(defaults.reclaim_first),
        ),
        hard_limit: writer.one(
            "memory.hard_limit",
            request.memory_hard_limit.map(muru_config::format_size),
            given.hard_limit,
            None,
        ),
        check_interval: writer.one(
            "memory.check_interval",
            request.memory_check_interval_ms.map(format_ms),
            given.check_interval,
            Some(format_ms(defaults.check_interval_ms)),
        ),
    };

    // The environment by name, the request's over the table's, secrets
    // as the references they are.

    let mut env = table.env;

    for name in env.keys() {
        writer.origins.insert(format!("env.{name}"), Origin::SOURCE);
    }

    for (name, value) in env_from(request) {
        writer.origins.insert(format!("env.{name}"), Origin::FLAGS);
        env.insert(name, value);
    }

    let sendfile_roots = if request.sendfile_roots.is_empty() {
        writer.whole("sendfile_roots", !table.sendfile_roots.is_empty());
        table.sendfile_roots
    } else {
        writer
            .origins
            .insert("sendfile_roots".to_owned(), Origin::FLAGS);
        request.sendfile_roots.clone()
    };

    writer.whole("reflection", table.reflection.is_some());
    writer.whole("handoff", table.handoff.is_some());
    writer.whole("grpc_web_origins", !table.grpc_web_origins.is_empty());
    writer.whole("scaling", table.scaling.is_some());
    writer.whole("groups", !table.groups.is_empty());
    writer.whole("mounts", !table.mounts.is_empty());
    writer.whole("logging", table.logging.is_some());
    writer.whole("max_body_size", table.max_body_size.is_some());
    writer.whole("error_pages", !table.error_pages.is_empty());
    writer.whole("error_override", table.error_override.is_some());
    writer.whole("maintenance", table.maintenance.is_some());
    writer.whole("commands", !table.commands.is_empty());

    ReleaseTable {
        kind: None,
        profile: None,
        command: Vec::new(),
        cwd: None,
        bind: None,
        instances: None,
        readiness: None,
        stop_signal: None,
        source: Some(source.to_owned()),
        entrypoint,
        interface: writer.one(
            "interface",
            parsed(&request.interface),
            table.interface,
            Some(muru_config::Interface::Auto),
        ),
        reflection: table.reflection,
        handoff: table.handoff,
        grpc_web_origins: table.grpc_web_origins,
        factory,
        factory_phase,
        python,
        build: (build != muru_config::Build::default()).then_some(build),
        bytecode: writer.one(
            "bytecode",
            parsed(&request.bytecode),
            table.bytecode,
            Some(muru_config::Bytecode::default()),
        ),
        processes: writer.one(
            "processes",
            count(request.processes),
            table.processes,
            Some(config::DEFAULT_PROCESSES as usize),
        ),
        threads: writer.one(
            "threads",
            count(request.threads),
            table.threads,
            Some(config::DEFAULT_THREADS as usize),
        ),
        sync_threads: writer.one(
            "sync_threads",
            count(request.sync_threads),
            table.sync_threads,
            Some(config::DEFAULT_SYNC_THREADS as usize),
        ),
        concurrency: writer.one(
            "concurrency",
            count(request.concurrency),
            table.concurrency,
            Some(config::DEFAULT_CONCURRENCY as usize),
        ),
        scaling: table.scaling,
        preload: writer.one(
            "preload",
            parsed(&request.preload),
            table.preload,
            Some(muru_config::Preload::Auto),
        ),
        hooks: writer.given("hooks", text(&request.hooks), table.hooks),
        groups: table.groups,
        mounts: table.mounts,
        static_dirs,
        logging: table.logging,
        max_requests: writer.one(
            "max_requests",
            request.max_requests,
            table.max_requests,
            None,
        ),
        max_lifetime: writer.one(
            "max_lifetime",
            request.max_lifetime_ms.map(format_ms),
            table.max_lifetime,
            None,
        ),
        timeouts: (timeouts != muru_config::Timeouts::default()).then_some(timeouts),
        max_body_size: table.max_body_size,
        memory: (memory != muru_config::Memory::default()).then_some(memory),
        env,
        accel_redirect: writer.one(
            "accel_redirect",
            request.accel_redirect,
            table.accel_redirect,
            Some(false),
        ),
        cgi_local_redirect: writer.one(
            "cgi_local_redirect",
            request.cgi_local_redirect,
            table.cgi_local_redirect,
            Some(false),
        ),
        sendfile_roots,
        sendfile_header: writer.given(
            "sendfile_header",
            request.sendfile_header.clone(),
            table.sendfile_header,
        ),
        error_pages: table.error_pages,
        error_override: table.error_override,
        maintenance: table.maintenance,
        commands: table.commands,
    }
}

/// A release that runs an HTTP server of a profile (section 9.9), which
/// takes its settings from the request alone.
fn managed(
    request: &pb::DeployRequest,
    source: &str,
    spec: &ReleaseSpec,
    writer: &mut Writer,
) -> Result<ReleaseTable, String> {
    let given = config::kind_table_from(request)?;
    let http = spec.http.as_ref().expect("a managed release has a server");

    for key in ["profile", "command", "stop_signal", "sendfile_header"] {
        writer.origins.insert(key.to_owned(), Origin::FLAGS);
    }

    if !given.sendfile_roots.is_empty() {
        writer
            .origins
            .insert("sendfile_roots".to_owned(), Origin::FLAGS);
    }

    let path = writer.given(
        "readiness.path",
        given
            .readiness
            .as_ref()
            .and_then(|table| table.path.clone()),
        None,
    );
    let timeout = writer.one(
        "readiness.timeout",
        request.readiness_timeout_ms.map(format_ms),
        None,
        Some(format_ms(http.readiness_timeout_ms)),
    );
    let shutdown_timeout = writer.one(
        "timeouts.shutdown_timeout",
        request.shutdown_timeout_ms.map(format_ms),
        None,
        Some(format_ms(spec.timeouts.shutdown_timeout_ms)),
    );
    let mut env = BTreeMap::new();

    for (name, value) in env_from(request) {
        writer.origins.insert(format!("env.{name}"), Origin::FLAGS);
        env.insert(name, value);
    }

    Ok(ReleaseTable {
        kind: Some(muru_config::ReleaseKind::Http),
        profile: given.profile,
        command: given.command,
        cwd: Some(source.to_owned()),
        bind: writer.one("bind", given.bind, None, Some(http.bind)),
        instances: writer.one("instances", given.instances, None, Some(http.instances)),
        readiness: (path.is_some() || timeout.is_some())
            .then_some(muru_config::Readiness { path, timeout }),
        stop_signal: given.stop_signal,
        timeouts: shutdown_timeout.map(|shutdown_timeout| muru_config::Timeouts {
            shutdown_timeout: Some(shutdown_timeout),
            ..muru_config::Timeouts::default()
        }),
        env,
        sendfile_roots: given.sendfile_roots,
        sendfile_header: given.sendfile_header,
        ..ReleaseTable::default()
    })
}

/// The request's environment as a table writes it: a value, or a secret
/// as the reference it is.
fn env_from(request: &pb::DeployRequest) -> Vec<(String, muru_config::EnvValue)> {
    request
        .env
        .iter()
        .map(|entry| {
            let value = if entry.secret_store.is_empty() {
                muru_config::EnvValue::Plain(entry.value.clone())
            } else {
                muru_config::EnvValue::Secret(muru_config::SecretValue {
                    secret: muru_config::SecretRef {
                        store: entry.secret_store.clone(),
                        key: entry.secret_key.clone(),
                    },
                    deliver: entry.as_file.then(|| "file".to_owned()),
                })
            };

            (entry.name.clone(), value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::provision::Provisioning;

    const SETTINGS: &str = r#"[tool.muru]
entrypoint = 'shop.wsgi:application'
factory = true
factory_phase = 'host'
threads = 6
preload = 'true'
python = '3.13'
max_requests = 5000
max_body_size = '8MB'
sendfile_roots = ['/srv/media']

[tool.muru.build]
extras = ['postgres']
compile_bytecode = false

[tool.muru.timeouts]
shutdown_timeout = '9s'
request_deadline = '40s'

[tool.muru.memory]
soft_limit = '600MB'

[tool.muru.env]
MODE = 'production'
REGION = 'here'

[[tool.muru.static]]
path = '/static/'
dir = 'assets'

[[tool.muru.static]]
path = '/media/'
dir = 'assets'
fallback = 'index.html'

[tool.muru.groups.reports]
paths = ['/reports/']
processes = 2

[tool.muru.mounts.api]
path = '/api'
entrypoint = 'shop.api:application'

[tool.muru.scaling]
mode = 'spare'
min_workers = 2
max_workers = 6

[tool.muru.commands.migrate]
run = ['python', 'manage.py', 'migrate']
"#;

    fn project(settings: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("pyproject.toml"), settings).unwrap();
        std::fs::create_dir(directory.path().join("assets")).unwrap();
        std::fs::create_dir(directory.path().join("other")).unwrap();
        directory
    }

    fn request(source: &Path) -> pb::DeployRequest {
        pb::DeployRequest {
            app: "shop".to_owned(),
            source: source.display().to_string(),
            ..Default::default()
        }
    }

    fn flags(source: &Path) -> pb::DeployRequest {
        pb::DeployRequest {
            processes: Some(4),
            interface: "wsgi".to_owned(),
            hooks: "shop.hooks".to_owned(),
            build_when: "never".to_owned(),
            build_extras: vec!["postgres".to_owned(), "redis".to_owned()],
            bytecode: "off".to_owned(),
            max_lifetime_ms: Some(3_600_000),
            soft_timeout_ms: Some(20_000),
            hard_timeout_ms: Some(30_000),
            memory_hard_limit: Some(900 * 1024 * 1024),
            memory_measure: "pss".to_owned(),
            sendfile_header: Some("X-Sendfile".to_owned()),
            static_dirs: vec![
                pb::StaticDir {
                    path: "/media".to_owned(),
                    dir: "other".to_owned(),
                },
                pb::StaticDir {
                    path: "/downloads/".to_owned(),
                    dir: "other".to_owned(),
                },
            ],
            env: vec![
                pb::EnvEntry {
                    name: "REGION".to_owned(),
                    value: "there".to_owned(),
                    ..Default::default()
                },
                pb::EnvEntry {
                    name: "TOKEN".to_owned(),
                    secret_store: "vault".to_owned(),
                    secret_key: "shop/token".to_owned(),
                    as_file: true,
                    ..Default::default()
                },
            ],
            ..request(source)
        }
    }

    /// The release an apply of the rendered table gives, and the one a
    /// fresh deploy of the request gives, with where their settings came
    /// from set aside: the two name the layers differently.
    fn both(request: &pb::DeployRequest, full: bool) -> (ReleaseSpec, ReleaseSpec, Rendered) {
        let off = Provisioning::off();
        let rendered = render(request, &request.source, full).unwrap();
        let mut applied =
            config::resolve_declared("shop", &rendered.table, &request.source, &off).unwrap();
        let mut deployed =
            config::resolve_spec(request, None, &config::Tables::default(), &off).unwrap();
        applied.layers = crate::model::Layers::default();
        deployed.layers = crate::model::Layers::default();
        (applied, deployed, rendered)
    }

    #[test]
    fn a_rendered_table_applies_as_the_release_a_fresh_deploy_gives() {
        let directory = project(SETTINGS);

        for request in [request(directory.path()), flags(directory.path())] {
            for full in [false, true] {
                let (mut applied, deployed, _) = both(&request, full);

                // The interpreter is named as the request or the settings
                // name it, which a deploy on the node resolves; here
                // nothing resolves it.

                assert_eq!(applied.python, Path::new("3.13"));
                applied.python = deployed.python.clone();
                assert_eq!(applied, deployed, "full: {full}");
            }
        }
    }

    #[test]
    fn the_flags_come_before_the_settings_and_the_defaults_are_left_out() {
        let directory = project(SETTINGS);
        let rendered = render(&flags(directory.path()), "releases/42", false).unwrap();
        let table = &rendered.table;

        assert_eq!(table.source.as_deref(), Some("releases/42"));
        assert_eq!(table.processes, Some(4));
        assert_eq!(table.threads, Some(6));
        assert_eq!(table.sync_threads, None);
        assert_eq!(table.python.as_deref(), Some("3.13"));
        assert_eq!(rendered.origins["processes"], Origin::FLAGS);
        assert_eq!(rendered.origins["threads"], Origin::SOURCE);
        assert!(!rendered.origins.contains_key("sync_threads"));
        assert_eq!(rendered.origins["groups"], Origin::SOURCE);
        assert_eq!(
            rendered.file.as_deref(),
            Some(
                directory
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("pyproject.toml")
            )
            .as_deref()
        );

        // A list the flags add to holds each name once, and says both.

        let build = table.build.as_ref().unwrap();
        assert_eq!(build.extras, ["postgres", "redis"]);
        assert_eq!(build.compile_bytecode, Some(false));
        assert_eq!(
            rendered.origins["build.extras"],
            Origin {
                source: true,
                flags: true
            }
        );

        // The environment and the directories of files are merged by
        // name and by prefix, and a secret stays a reference.

        assert_eq!(rendered.origins["env.MODE"], Origin::SOURCE);
        assert_eq!(rendered.origins["env.REGION"], Origin::FLAGS);
        assert_eq!(
            table.env["REGION"],
            muru_config::EnvValue::Plain("there".to_owned())
        );
        assert!(matches!(
            &table.env["TOKEN"],
            muru_config::EnvValue::Secret(secret)
                if secret.secret.store == "vault" && secret.deliver.as_deref() == Some("file")
        ));
        let dirs: Vec<(&str, &str)> = table
            .static_dirs
            .iter()
            .map(|entry| (entry.path.as_str(), entry.dir.as_str()))
            .collect();
        assert_eq!(
            dirs,
            [
                ("/static/", "assets"),
                ("/media", "other"),
                ("/downloads/", "other")
            ]
        );
        assert_eq!(table.static_dirs[1].fallback, None);

        // The table's own timeouts stay beside the flags'.

        let timeouts = table.timeouts.as_ref().unwrap();
        assert_eq!(timeouts.soft_timeout.as_deref(), Some("20s"));
        assert_eq!(timeouts.shutdown_timeout.as_deref(), Some("9s"));
        assert_eq!(timeouts.request_deadline.as_deref(), Some("40s"));
        assert_eq!(timeouts.gil_stall_timeout, None);
    }

    #[test]
    fn full_writes_the_defaults_and_names_what_has_none() {
        let directory = project("[tool.muru]\nentrypoint = 'app:application'\n");
        let source = directory.path();
        let plain = render(&request(source), ".", false).unwrap();
        assert_eq!(
            plain.table,
            ReleaseTable {
                source: Some(".".to_owned()),
                entrypoint: "app:application".to_owned(),
                ..ReleaseTable::default()
            }
        );

        let full = render(&request(source), ".", true).unwrap();
        let table = &full.table;
        assert_eq!(table.processes, Some(1));
        assert_eq!(table.threads, Some(4));
        assert_eq!(table.sync_threads, Some(8));
        assert_eq!(table.concurrency, Some(1000));
        assert_eq!(table.interface, Some(muru_config::Interface::Auto));
        assert_eq!(table.accel_redirect, Some(false));
        assert_eq!(full.origins["processes"], Origin::DEFAULT);
        assert_eq!(full.origins["entrypoint"], Origin::SOURCE);
        assert_eq!(table.python, None);
        assert_eq!(
            table.timeouts.as_ref().unwrap().shutdown_timeout.as_deref(),
            Some("5s")
        );
        assert_eq!(
            table.memory.as_ref().unwrap().measure,
            Some(muru_config::Measure::Uss)
        );
        assert_eq!(
            full.unset,
            [
                "python",
                "timeouts.soft_timeout",
                "timeouts.hard_timeout",
                "memory.soft_limit",
                "memory.hard_limit",
                "max_requests",
                "max_lifetime"
            ]
        );

        // Every setting `config show --effective` lists is written or
        // named as unset.

        let spec = config::resolve_spec(
            &request(source),
            None,
            &config::Tables::default(),
            &Provisioning::off(),
        )
        .unwrap();

        for (key, _) in config::release_settings(&spec) {
            assert!(
                full.origins.contains_key(key) || full.unset.contains(&key),
                "{key} is neither written nor named"
            );
        }
    }

    #[test]
    fn a_flags_entrypoint_leaves_the_tables_factory_behind() {
        let directory = project(SETTINGS);
        let rendered = render(
            &pb::DeployRequest {
                entrypoint: "other:application".to_owned(),
                ..request(directory.path())
            },
            ".",
            false,
        )
        .unwrap();
        assert_eq!(rendered.table.entrypoint, "other:application");
        assert_eq!(rendered.table.factory, None);
        assert_eq!(rendered.table.factory_phase, None);

        let kept = render(&request(directory.path()), ".", false).unwrap();
        assert_eq!(kept.table.factory, Some(true));
        assert_eq!(
            kept.table.factory_phase,
            Some(muru_config::FactoryPhase::Host)
        );
    }

    #[test]
    fn a_managed_server_is_written_from_the_flags_alone() {
        let directory = project(SETTINGS);
        let request = pb::DeployRequest {
            kind: "http".to_owned(),
            profile: "gunicorn".to_owned(),
            command: vec!["gunicorn".to_owned(), "shop.wsgi".to_owned()],
            instances: Some(2),
            readiness_path: "/healthz".to_owned(),
            env: vec![pb::EnvEntry {
                name: "MODE".to_owned(),
                value: "managed".to_owned(),
                ..Default::default()
            }],
            ..request(directory.path())
        };

        for full in [false, true] {
            let (applied, deployed, rendered) = both(&request, full);
            assert_eq!(applied, deployed, "full: {full}");
            let table = &rendered.table;
            assert_eq!(table.kind, Some(muru_config::ReleaseKind::Http));
            assert_eq!(table.cwd.as_deref(), Some(request.source.as_str()));
            assert_eq!(table.source, None);
            assert_eq!(table.instances, Some(2));
            assert_eq!(table.entrypoint, "");
            assert_eq!(table.bind.is_some(), full);
            assert_eq!(rendered.file, None);
        }
    }

    #[test]
    fn what_a_deploy_refuses_is_refused() {
        let directory = project(SETTINGS);
        let error = render(
            &pb::DeployRequest {
                soft_timeout_ms: Some(30_000),
                hard_timeout_ms: Some(31_000),
                ..request(directory.path())
            },
            ".",
            false,
        )
        .unwrap_err();
        assert!(error.contains("hard_timeout"), "{error}");

        let empty = tempfile::tempdir().unwrap();
        let error = render(&request(empty.path()), ".", false).unwrap_err();
        assert!(error.contains("has no entrypoint"), "{error}");
    }
}
