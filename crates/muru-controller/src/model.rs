//! The objects the controller manages: applications and releases (design
//! specification sections 3 and 9.1). The first slice holds what a deploy
//! of one application needs; routes, hosts and the rest arrive with their
//! features.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use muru_config::{FactoryPhase, Interface, Preload, ScalingMode, ServerBind, ServerProfile};
use serde::{Deserialize, Serialize};

/// The states of section 9.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseState {
    /// Recorded, not yet started.
    Pending,
    /// The environment is being built from the lock (section 8.2).
    Building,
    /// The host is starting and preloading.
    Starting,
    /// Workers are forking and not yet all ready, or all are ready and
    /// the edge has not yet been given the release.
    Warming,
    /// Every worker is ready and the edge has the release, so it can
    /// take traffic.
    Ready,
    /// Removed from traffic; requests in flight are finishing.
    Draining,
    /// Drained and kept warm for the retention window, so a rollback to it
    /// is instant (section 9.1).
    Retained,
    /// Hosts and workers are being stopped.
    Stopping,
    /// Stopped, and can be started again (a rollback target).
    Stopped,
    /// Failed to start or to stay up; `message` says why.
    Failed,
}

impl ReleaseState {
    /// Every state, for parsing.
    pub const ALL: [ReleaseState; 10] = [
        ReleaseState::Pending,
        ReleaseState::Building,
        ReleaseState::Starting,
        ReleaseState::Warming,
        ReleaseState::Ready,
        ReleaseState::Draining,
        ReleaseState::Retained,
        ReleaseState::Stopping,
        ReleaseState::Stopped,
        ReleaseState::Failed,
    ];

    /// The name as the API and the CLI show it.
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseState::Pending => "pending",
            ReleaseState::Building => "building",
            ReleaseState::Starting => "starting",
            ReleaseState::Warming => "warming",
            ReleaseState::Ready => "ready",
            ReleaseState::Draining => "draining",
            ReleaseState::Retained => "retained",
            ReleaseState::Stopping => "stopping",
            ReleaseState::Stopped => "stopped",
            ReleaseState::Failed => "failed",
        }
    }

    /// Whether the release has processes: a host that may serve or drain.
    pub fn is_running(self) -> bool {
        matches!(
            self,
            ReleaseState::Starting
                | ReleaseState::Warming
                | ReleaseState::Ready
                | ReleaseState::Draining
                | ReleaseState::Retained
                | ReleaseState::Stopping
        )
    }

    /// Whether nothing further happens to the release on its own.
    pub fn is_final(self) -> bool {
        matches!(self, ReleaseState::Stopped | ReleaseState::Failed)
    }
}

impl fmt::Display for ReleaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReleaseState {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        ReleaseState::ALL
            .into_iter()
            .find(|state| state.as_str() == text)
            .ok_or_else(|| format!("{text:?} is not a release state"))
    }
}

/// A directory of a release's own files (section 4.9), as the edge
/// serves it: resolved under the source and checked to exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticSpec {
    /// The path prefix.
    pub path: String,
    /// The directory, absolute.
    pub dir: PathBuf,
    /// The files tried, in order, when a directory is requested.
    pub index: Vec<String>,
    /// The file served for paths with no file of their own.
    pub fallback_file: Option<String>,
    /// `max-age`, in seconds, when set.
    pub cache_max_age_s: Option<u64>,
    /// Whether `immutable` is added.
    pub cache_immutable: bool,
    /// Files that get `no-cache` instead, by pattern.
    pub cache_except: Vec<String>,
    /// Whether a directory asked for without a trailing slash is
    /// redirected to the slash form.
    pub redirect_slash: bool,
    /// Whether names may differ from the file's on disk in case.
    pub case_insensitive: bool,
}

impl StaticSpec {
    /// The entry as the snapshot carries it.
    pub fn message(&self) -> muru_proto::edge::v1::StaticDir {
        muru_proto::edge::v1::StaticDir {
            path: self.path.clone(),
            dir: self.dir.display().to_string(),
            index: self.index.clone(),
            fallback_file: self.fallback_file.clone().unwrap_or_default(),
            cache_max_age_s: self.cache_max_age_s.unwrap_or(0),
            cache_immutable: self.cache_immutable,
            cache_except: self.cache_except.clone(),
            no_redirect_slash: !self.redirect_slash,
            case_insensitive: self.case_insensitive,
        }
    }

    /// The entry as a declared release's table has it, the directory
    /// absolute.
    pub fn table(&self) -> muru_config::StaticDir {
        muru_config::StaticDir {
            path: self.path.clone(),
            dir: self.dir.display().to_string(),
            index: self.index.clone(),
            fallback: self.fallback_file.clone(),
            cache: (self.cache_max_age_s.is_some()
                || self.cache_immutable
                || !self.cache_except.is_empty())
            .then(|| muru_config::CacheTable {
                max_age: self.cache_max_age_s.map(|seconds| {
                    muru_config::format_duration(std::time::Duration::from_secs(seconds))
                }),
                immutable: self.cache_immutable.then_some(true),
                except: self.cache_except.clone(),
            }),
            redirect_slash: (!self.redirect_slash).then_some(false),
            case: self.case_insensitive.then(|| "insensitive".to_owned()),
        }
    }
}

/// A mount of a release (section 6.7): a path served by another
/// entrypoint in the same workers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    /// The mount's name.
    pub name: String,
    /// The path prefix.
    pub path: String,
    /// The application as module:callable.
    pub entrypoint: String,
    /// The interface, auto when detected.
    pub interface: Interface,
    /// Reachable only through internal redirects (section 4.10).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub internal: bool,
    /// Other applications whose redirects may reach the mount when it is
    /// internal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept_redirects_from: Vec<String>,
    /// X-Accel-Redirect from the mount is an internal redirect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accel_redirect: bool,
    /// A 200 with a local Location and no body is an internal redirect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cgi_local_redirect: bool,
    /// An ASGI mount's requests at once on each worker, slots of its own
    /// at the edge (section 6.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    /// An ASGI mount's open WebSockets at once on each worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    /// Directories the mount's X-Muru-Sendfile paths must lie in
    /// (section 4.9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sendfile_roots: Vec<String>,
    /// A second header honoured as X-Muru-Sendfile is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sendfile_header: Option<String>,
    /// Threads of the mount's own; the worker's shared pool when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<muru_config::MountThreads>,
}

/// A worker group of a release (section 7.1): its paths and sizes. The
/// default group is the release's own settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSpec {
    /// The group's name.
    pub name: String,
    /// Path prefixes it serves, with an optional trailing `*`.
    pub paths: Vec<String>,
    /// The named mounts its workers serve beside the default mount;
    /// every mount when empty (ADR 0033).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
    /// The client classes it takes (section 4.14), ahead of the groups
    /// that name none; their requests for every path without `paths`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_class: Vec<String>,
    /// The hosts it takes, lower case; every host when empty (ADR 0033).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host: Vec<String>,
    /// The methods it takes, upper case; every method when empty (ADR
    /// 0033).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub method: Vec<String>,
    /// Whether the release is ready without the group's workers (ADR
    /// 0033).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// Worker processes.
    pub processes: u32,
    /// Application threads per worker.
    pub threads: u32,
    /// Threads for synchronous code called from an ASGI application.
    pub sync_threads: u32,
    /// Requests an ASGI worker's event loop takes at once.
    pub concurrency: u32,
    /// How the group is scaled (sections 7.2 and 7.4).
    #[serde(default)]
    pub scaling: ScalingSpec,
    /// The group's own timeouts, each the release's when absent (section
    /// 7.1).
    #[serde(default, skip_serializing_if = "GroupTimeouts::is_empty")]
    pub timeouts: GroupTimeouts,
    /// The group's own memory policy, resolved over the release's; the
    /// release's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryPolicy>,
}

/// A worker group's own timeouts (section 7.1): the ones a group may
/// set, each the release's when absent. The stop budget is the release's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupTimeouts {
    /// How long a request may run before it is interrupted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_timeout_ms: Option<u64>,
    /// How long a request may run before its worker is replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_timeout_ms: Option<u64>,
    /// How long a replaced worker gets to finish its other requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_drain_grace_ms: Option<u64>,
    /// How long the event loop may go without running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_stall_timeout_ms: Option<u64>,
    /// How long no thread may run Python, or no report arrive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gil_stall_timeout_ms: Option<u64>,
}

impl GroupTimeouts {
    /// Whether the group sets nothing of its own.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The timeouts in force for the group: its own over the release's.
    pub fn resolve(&self, release: &WorkerTimeouts) -> WorkerTimeouts {
        WorkerTimeouts {
            soft_timeout_ms: self.soft_timeout_ms.or(release.soft_timeout_ms),
            hard_timeout_ms: self.hard_timeout_ms.or(release.hard_timeout_ms),
            hard_drain_grace_ms: self
                .hard_drain_grace_ms
                .unwrap_or(release.hard_drain_grace_ms),
            shutdown_timeout_ms: release.shutdown_timeout_ms,
            websocket_drain_grace_ms: release.websocket_drain_grace_ms,
            loop_stall_timeout_ms: self
                .loop_stall_timeout_ms
                .unwrap_or(release.loop_stall_timeout_ms),
            gil_stall_timeout_ms: self
                .gil_stall_timeout_ms
                .unwrap_or(release.gil_stall_timeout_ms),
        }
    }

    /// The group's own timeouts as a `timeouts` table, for an export.
    pub fn table(&self) -> Option<muru_config::Timeouts> {
        if self.is_empty() {
            return None;
        }

        let format = |ms: Option<u64>| {
            ms.map(|ms| muru_config::format_duration(std::time::Duration::from_millis(ms)))
        };

        Some(muru_config::Timeouts {
            soft_timeout: format(self.soft_timeout_ms),
            hard_timeout: format(self.hard_timeout_ms),
            hard_drain_grace: format(self.hard_drain_grace_ms),
            shutdown_timeout: None,
            websocket_drain_grace: None,
            loop_stall_timeout: format(self.loop_stall_timeout_ms),
            gil_stall_timeout: format(self.gil_stall_timeout_ms),
        })
    }
}

/// What a release runs: its source and the settings fixed at creation
/// (ADR 0003). Scale is the one thing that changes live, later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseSpec {
    /// The source directory.
    pub source: PathBuf,
    /// The application as module:callable.
    pub entrypoint: String,
    /// The interface, auto when detected.
    pub interface: Interface,
    /// The interpreter.
    pub python: PathBuf,
    /// Worker processes.
    pub processes: u32,
    /// Application threads per worker.
    pub threads: u32,
    /// Threads for synchronous code called from an ASGI application.
    pub sync_threads: u32,
    /// Requests an ASGI worker's loop takes at once.
    pub concurrency: u32,
    /// The preload mode.
    pub preload: Preload,
    /// The hooks module, if any.
    pub hooks: Option<String>,
    /// Whether the entrypoint is a factory, and when it is called.
    pub factory: Option<FactoryPhase>,
    /// The origins whose pages may call it through gRPC-Web, answered
    /// with CORS (section 6.13).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grpc_web_origins: Vec<String>,
    /// Whether gRPC server reflection is served (section 6.13).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reflection: bool,
    /// Named worker groups beside the default one.
    #[serde(default)]
    pub groups: Vec<GroupSpec>,
    /// Named mounts beside the default one at /.
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    /// The release's own directories of files (section 4.9), served by
    /// the edge from under its source; none for a release recorded
    /// before they existed.
    #[serde(default)]
    pub static_dirs: Vec<StaticSpec>,
    /// Python logging defaults (section 11.4).
    #[serde(default)]
    pub logging: muru_config::PythonLogging,
    /// Requests a worker serves before it is replaced (section 7.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    /// How long a worker lives before it is replaced, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime_ms: Option<u64>,
    /// The timeouts the host enforces on its workers (section 7.6).
    #[serde(default)]
    pub timeouts: WorkerTimeouts,
    /// The memory policy for the workers (section 7.7).
    #[serde(default)]
    pub memory: MemoryPolicy,
    /// Environment variables for the workers (section 10.8), in name
    /// order; secret values are never recorded, only their references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvSpec>,
    /// How the default group is scaled (sections 7.2 and 7.4).
    #[serde(default)]
    pub scaling: ScalingSpec,
    /// The default mount honours X-Accel-Redirect (section 4.10).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accel_redirect: bool,
    /// The default mount treats a 200 with a local Location and no body
    /// as an internal redirect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cgi_local_redirect: bool,
    /// Directories the default mount's X-Muru-Sendfile paths must lie in
    /// (section 4.9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sendfile_roots: Vec<String>,
    /// A second header honoured as X-Muru-Sendfile is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sendfile_header: Option<String>,
    /// Custom error pages (section 4.12), by status or status:reason; a
    /// relative file is taken from the source directory.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub error_pages: BTreeMap<String, muru_config::ErrorPage>,
    /// Which of the application's own error responses the edge replaces
    /// with error pages (section 4.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_override: Option<muru_config::ErrorOverride>,
    /// What maintenance mode lets through (section 4.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<muru_config::Maintenance>,
    /// Section 9.9: the HTTP server the release runs instead of workers,
    /// when it is a managed HTTP release; `source` is then the directory
    /// its command runs in, and the worker settings are unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpServerSpec>,
    /// Section 3.3: the layer each setting came from, by key.
    #[serde(default, skip_serializing_if = "Layers::is_empty")]
    pub layers: Layers,
    /// Section 8.1: what the interpreter was found to be when the release
    /// was resolved, and the request that chose it; none for a release
    /// recorded before probing existed or resolved without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<Interpreter>,
    /// Section 8.2: when and how the environment is built on the node.
    #[serde(default)]
    pub build: BuildSpec,
    /// Section 8.2: where compiled bytecode goes.
    #[serde(default)]
    pub bytecode: BytecodeSpec,
}

/// Section 3.3: where each of a release's settings came from, by key:
/// the release table or deploy flags, the source's [tool.muru] table,
/// the previous release, or the default. It records how the settings
/// were reached, not what they are, so two specs are equal whatever
/// their layers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Layers(pub BTreeMap<String, String>);

impl Layers {
    /// Whether nothing is recorded, as for a release made before
    /// layers were.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PartialEq for Layers {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Layers {}

/// Section 8.2: where a release's bytecode goes. `precompiled` records
/// what was found in the source at resolution, so it stays out of the
/// comparison that keeps releases immutable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BytecodeSpec {
    /// The setting.
    #[serde(default)]
    pub mode: muru_config::Bytecode,
    /// Whether the source holds bytecode for the interpreter's cache tag,
    /// in which case a redirected release runs on it instead.
    #[serde(default)]
    pub precompiled: bool,
}

impl BytecodeSpec {
    /// What the release runs with: the setting, except that redirecting
    /// gives way to precompiled bytecode in the source.
    pub fn effective(&self) -> muru_config::Bytecode {
        match self.mode {
            muru_config::Bytecode::Redirect if self.precompiled => muru_config::Bytecode::Source,
            mode => mode,
        }
    }

    /// A short description, as `redirect`, `source (precompiled)` or `off`.
    pub fn describe(&self) -> String {
        match (self.mode, self.precompiled) {
            (muru_config::Bytecode::Redirect, true) => "source (precompiled)".to_owned(),
            (mode, _) => mode.as_str().to_owned(),
        }
    }
}

impl PartialEq for BytecodeSpec {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
    }
}

impl Eq for BytecodeSpec {}

/// Section 8.2: the build table as resolved for a release, and whether
/// the daemon builds an environment for it. `builds` follows from the
/// mode and the source as they were at resolution, so it stays out of
/// the comparison that keeps releases immutable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildSpec {
    /// The mode in force.
    #[serde(default)]
    pub when: muru_config::BuildWhen,
    /// Extras the environment includes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extras: Vec<String>,
    /// Dependency groups beyond the defaults.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Whether sdists may be built on the node.
    #[serde(default)]
    pub source_builds: bool,
    /// Whether bytecode is compiled at build time.
    #[serde(default = "default_true")]
    pub compile_bytecode: bool,
    /// Whether the daemon builds an environment for the release: the mode
    /// says so and the source has a lock.
    #[serde(default)]
    pub builds: bool,
}

impl PartialEq for BuildSpec {
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when
            && self.extras == other.extras
            && self.groups == other.groups
            && self.source_builds == other.source_builds
            && self.compile_bytecode == other.compile_bytecode
    }
}

impl Eq for BuildSpec {}

/// Section 8.1: the interpreter as the probe found it when the release
/// was resolved. It records what was found, not a setting, so two specs
/// are equal whatever their probes say.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Interpreter {
    /// The version, as `3.13.9`.
    pub version: String,
    /// The wheel tag, as `cp313` or `cp314t`.
    pub tag: String,
    /// `sys.implementation.cache_tag`, as `cpython-313`, which names the
    /// bytecode files.
    #[serde(default)]
    pub cache_tag: String,
    /// Whether the GIL is disabled.
    #[serde(default)]
    pub free_threaded: bool,
    /// `sys.platform` and the machine, as `linux x86_64`.
    #[serde(default)]
    pub platform: String,
    /// The request that chose the interpreter and its source, as `3.13
    /// from .python-version`, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
}

impl Interpreter {
    /// What a resolution found.
    pub fn from_resolved(resolved: &muru_provision::Resolved) -> Self {
        Self {
            version: resolved.probe.version.clone(),
            tag: resolved.probe.tag(),
            cache_tag: resolved.probe.cache_tag.clone(),
            free_threaded: resolved.probe.free_threaded,
            platform: format!("{} {}", resolved.probe.platform, resolved.probe.machine),
            request: resolved
                .request()
                .map(|request| format!("{} from {}", request.text, request.source.as_str())),
        }
    }

    /// A short description, as `3.13.9, cp313, free-threaded`.
    pub fn describe(&self) -> String {
        let mut text = format!("{}, {}", self.version, self.tag);

        if self.free_threaded {
            text.push_str(", free-threaded");
        }

        text
    }
}

impl PartialEq for Interpreter {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Interpreter {}

/// Section 9.9: a release that runs an HTTP server of its own instead
/// of Muru workers, started from a command and routed to as an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpServerSpec {
    /// The server, which fixes how it binds and how it is stopped.
    pub profile: ServerProfile,
    /// The command, with `{bind}` where the server takes its address.
    pub command: Vec<String>,
    /// How each instance binds.
    pub bind: ServerBind,
    /// How many instances run.
    pub instances: u32,
    /// The path probed until an instance answers 2xx or 3xx; any answer
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_path: Option<String>,
    /// How long the instances get to be ready, in milliseconds.
    pub readiness_timeout_ms: u64,
    /// The signal that stops an instance gracefully, when the release
    /// names one; the profile's otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
}

impl HttpServerSpec {
    /// The server and its instances as a phrase: `gunicorn, 2 instances`.
    pub fn describe(&self) -> String {
        format!(
            "{}, {} instance{}",
            self.profile.as_str(),
            self.instances,
            if self.instances == 1 { "" } else { "s" }
        )
    }
}

/// One environment variable of a release (section 10.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvSpec {
    /// The variable's name.
    pub name: String,
    /// Its value as written, when it is not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The store a secret value comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_store: Option<String>,
    /// The key of the secret in its store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    /// Whether the secret is delivered as a file named by NAME_FILE.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub as_file: bool,
}

impl EnvSpec {
    /// The variable as a phrase that never shows a secret's value.
    pub fn describe(&self) -> String {
        match (&self.secret_store, &self.secret_key) {
            (Some(store), Some(key)) => format!(
                "{}=secret {store}/{key}{}",
                self.name,
                if self.as_file { " as file" } else { "" }
            ),
            _ => format!("{}={}", self.name, self.value.clone().unwrap_or_default()),
        }
    }
}

/// How a worker group is scaled (sections 7.2 and 7.4), resolved with
/// the defaults of appendix C; durations in milliseconds. The default is
/// a fixed count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScalingSpec {
    /// static, spare or target.
    pub mode: ScalingMode,
    /// The fewest workers.
    pub min_workers: u32,
    /// The most workers; the fixed count in static mode.
    pub max_workers: u32,
    /// Spare mode: scale out below this many idle workers.
    pub min_spare: u32,
    /// Spare mode: scale in above this many idle workers.
    pub max_spare: u32,
    /// Target mode: the percentage of slots to keep busy.
    pub slot_utilization_percent: u32,
    /// Target mode: scale out while the p95 queue wait is over this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_wait_p95_ms: Option<u64>,
    /// How long a scale-out condition must hold.
    pub scale_out_window_ms: u64,
    /// How long every scale-in condition must hold.
    pub scale_in_window_ms: u64,
    /// No scale-in this long after a scale-out.
    pub hold_after_growth_ms: u64,
    /// At most one worker reaped per this interval.
    pub reap_interval_ms: u64,
    /// A wholly idle group scales in after this.
    pub idle_reap_after_ms: u64,
    /// No worker younger than this is reaped; from the measured time to
    /// ready when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_lifetime_ms: Option<u64>,
    /// The most workers one scale-out adds.
    pub max_step_up: u32,
    /// Reaps allowed per hour per release.
    pub churn_budget: u32,
    /// Decide and report without acting.
    pub advisory: bool,
    /// How long the host stays once every group has scaled to zero, for
    /// the default group's spec; it stays while the release runs when
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_idle_timeout_ms: Option<u64>,
}

impl Default for ScalingSpec {
    fn default() -> Self {
        Self {
            mode: ScalingMode::Static,
            min_workers: 1,
            max_workers: 0,
            min_spare: 1,
            max_spare: 3,
            slot_utilization_percent: 70,
            queue_wait_p95_ms: None,
            scale_out_window_ms: 5_000,
            scale_in_window_ms: 600_000,
            hold_after_growth_ms: 300_000,
            reap_interval_ms: 60_000,
            idle_reap_after_ms: 600_000,
            min_lifetime_ms: None,
            max_step_up: 4,
            churn_budget: 6,
            advisory: false,
            host_idle_timeout_ms: None,
        }
    }
}

impl ScalingSpec {
    /// Whether the count is fixed.
    pub fn is_static(&self) -> bool {
        self.mode == ScalingMode::Static
    }

    /// The policy as one phrase, for reports: `static`, or the mode with
    /// its bounds and what makes it move.
    pub fn describe(&self) -> String {
        match self.mode {
            ScalingMode::Static => "static".to_owned(),
            ScalingMode::Spare => format!(
                "spare {}-{} workers, {}-{} idle{}",
                self.min_workers,
                self.max_workers,
                self.min_spare,
                self.max_spare,
                if self.advisory { ", advisory" } else { "" }
            ),
            ScalingMode::Target => format!(
                "target {}-{} workers, {}% of slots busy{}{}",
                self.min_workers,
                self.max_workers,
                self.slot_utilization_percent,
                match self.queue_wait_p95_ms {
                    Some(ms) => format!(", queue wait p95 {ms} ms"),
                    None => String::new(),
                },
                if self.advisory { ", advisory" } else { "" }
            ),
        }
    }
}

/// The memory policy of a release's workers (section 7.7), in bytes and
/// milliseconds; the defaults are those of appendix C.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPolicy {
    /// What is measured.
    #[serde(default = "default_measure")]
    pub measure: muru_config::Measure,
    /// Over this for `soft_for_ms`, the worker reclaims and then
    /// recycles; off when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_limit: Option<u64>,
    /// How long a worker may stay over the soft limit.
    #[serde(default = "default_soft_for_ms")]
    pub soft_for_ms: u64,
    /// Whether to reclaim before recycling over the soft limit.
    #[serde(default = "default_true")]
    pub reclaim_first: bool,
    /// Over this the worker leaves routing at once and is replaced; off
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_limit: Option<u64>,
    /// How often memory is measured.
    #[serde(default = "default_check_interval_ms")]
    pub check_interval_ms: u64,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            measure: default_measure(),
            soft_limit: None,
            soft_for_ms: default_soft_for_ms(),
            reclaim_first: true,
            hard_limit: None,
            check_interval_ms: default_check_interval_ms(),
        }
    }
}

fn default_measure() -> muru_config::Measure {
    muru_config::Measure::Uss
}

fn default_soft_for_ms() -> u64 {
    120_000
}

fn default_true() -> bool {
    true
}

fn default_check_interval_ms() -> u64 {
    5_000
}

/// The timeouts a host enforces on its workers (section 7.6), in
/// milliseconds; the defaults are those of appendix C.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTimeouts {
    /// How long a request may run before it is interrupted; off when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_timeout_ms: Option<u64>,
    /// How long a request may run before its worker is replaced; off
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_timeout_ms: Option<u64>,
    /// How long a replaced worker gets to finish its other requests.
    #[serde(default = "default_hard_drain_grace_ms")]
    pub hard_drain_grace_ms: u64,
    /// How long a worker told to stop gets before it is killed, from
    /// when its WebSockets are closed.
    #[serde(default = "default_shutdown_timeout_ms")]
    pub shutdown_timeout_ms: u64,
    /// How long a worker told to stop keeps its open WebSockets before it
    /// closes them with 1001.
    #[serde(default)]
    pub websocket_drain_grace_ms: u64,
    /// How long an ASGI worker's event loop may go without running
    /// before the worker is replaced.
    #[serde(default = "default_loop_stall_timeout_ms")]
    pub loop_stall_timeout_ms: u64,
    /// How long a worker may go with no thread running Python, or with
    /// no report to its host, before it is replaced.
    #[serde(default = "default_gil_stall_timeout_ms")]
    pub gil_stall_timeout_ms: u64,
}

impl Default for WorkerTimeouts {
    fn default() -> Self {
        Self {
            soft_timeout_ms: None,
            hard_timeout_ms: None,
            hard_drain_grace_ms: default_hard_drain_grace_ms(),
            shutdown_timeout_ms: default_shutdown_timeout_ms(),
            websocket_drain_grace_ms: 0,
            loop_stall_timeout_ms: default_loop_stall_timeout_ms(),
            gil_stall_timeout_ms: default_gil_stall_timeout_ms(),
        }
    }
}

impl WorkerTimeouts {
    /// How long a worker told to stop may take before it is killed: the
    /// WebSockets' grace, then the shutdown timeout.
    pub fn stop_budget(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.websocket_drain_grace_ms
                .saturating_add(self.shutdown_timeout_ms),
        )
    }
}

impl MemoryPolicy {
    /// The policy a `memory` table makes of this one: each key the
    /// table sets replaces the value here (section 7.1, a group's policy
    /// over the release's).
    pub fn with_table(&self, table: &muru_config::Memory) -> Result<Self, String> {
        let size = |text: Option<&String>, name: &str| -> Result<Option<u64>, String> {
            text.map(|text| {
                muru_config::parse_size(text).map_err(|error| format!("memory.{name}: {error}"))
            })
            .transpose()
        };
        let duration = |text: Option<&String>, name: &str| -> Result<Option<u64>, String> {
            text.map(|text| {
                muru_config::parse_duration(text)
                    .map(|duration| duration.as_millis() as u64)
                    .map_err(|error| format!("memory.{name}: {error}"))
            })
            .transpose()
        };
        let policy = Self {
            measure: table.measure.unwrap_or(self.measure),
            soft_limit: size(table.soft_limit.as_ref(), "soft_limit")?.or(self.soft_limit),
            soft_for_ms: duration(table.soft_for.as_ref(), "soft_for")?.unwrap_or(self.soft_for_ms),
            reclaim_first: table.reclaim_first.unwrap_or(self.reclaim_first),
            hard_limit: size(table.hard_limit.as_ref(), "hard_limit")?.or(self.hard_limit),
            check_interval_ms: duration(table.check_interval.as_ref(), "check_interval")?
                .unwrap_or(self.check_interval_ms),
        };

        if let (Some(soft), Some(hard)) = (policy.soft_limit, policy.hard_limit)
            && hard < soft
        {
            return Err("memory.hard_limit must not be below memory.soft_limit".to_owned());
        }

        if policy.check_interval_ms == 0 {
            return Err("memory.check_interval must be longer than nothing".to_owned());
        }

        Ok(policy)
    }

    /// The policy as a `memory` table, every key spelled out.
    pub fn table(&self) -> muru_config::Memory {
        let duration = |ms: u64| muru_config::format_duration(std::time::Duration::from_millis(ms));

        muru_config::Memory {
            measure: Some(self.measure),
            soft_limit: self.soft_limit.map(muru_config::format_size),
            soft_for: Some(duration(self.soft_for_ms)),
            reclaim_first: Some(self.reclaim_first),
            hard_limit: self.hard_limit.map(muru_config::format_size),
            check_interval: Some(duration(self.check_interval_ms)),
        }
    }
}

fn default_hard_drain_grace_ms() -> u64 {
    10_000
}

fn default_shutdown_timeout_ms() -> u64 {
    5_000
}

fn default_loop_stall_timeout_ms() -> u64 {
    10_000
}

fn default_gil_stall_timeout_ms() -> u64 {
    30_000
}

/// Section 9.3: where a rollout is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutState {
    /// Shifting traffic step by step.
    Running,
    /// Held where it is by hand; resume continues.
    Paused,
    /// The next step waits for approval (resume).
    AwaitingApproval,
    /// The candidate serves everything; the rollout is over.
    Promoted,
    /// The candidate was taken out of traffic; the rollout is over.
    RolledBack,
}

impl RolloutState {
    /// The name as the API and the CLI show it.
    pub fn as_str(self) -> &'static str {
        match self {
            RolloutState::Running => "running",
            RolloutState::Paused => "paused",
            RolloutState::AwaitingApproval => "awaiting_approval",
            RolloutState::Promoted => "promoted",
            RolloutState::RolledBack => "rolled_back",
        }
    }

    /// Whether the rollout still shapes traffic.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            RolloutState::Running | RolloutState::Paused | RolloutState::AwaitingApproval
        )
    }
}

/// One step of a rollout: the candidate's share and how long it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutStep {
    /// Percent of the application's requests.
    pub percent: u32,
    /// How long the step lasts before the gates decide.
    pub hold_ms: u64,
}

/// Section 9.3: what the candidate must not exceed over a step, judged
/// against the baseline over the same window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutGates {
    /// The 5xx ratio.
    pub max_error_rate: f64,
    /// The 95th percentile of total request time, when set.
    pub max_p95_ms: Option<u64>,
    /// The 99th percentile of total request time, when set.
    #[serde(default)]
    pub max_p99_ms: Option<u64>,
    /// Worker crashes and timeout kills, which need no request count to
    /// judge.
    #[serde(default)]
    pub max_worker_faults: u64,
    /// The 95th percentile of the wait for a worker thread, when set.
    #[serde(default)]
    pub max_queue_wait_p95_ms: Option<u64>,
    /// URLs requested through the edge from each release in turn, by
    /// preview; one failing on the candidate alone rolls back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<String>,
    /// Requests the candidate must have served before the gates judge.
    pub min_requests: u64,
}

/// A rollout record (section 9.3): a candidate release taking a growing
/// share of its application's traffic from the baseline, step by step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rollout {
    /// The application.
    pub app: String,
    /// The id, unique within the application.
    pub id: String,
    /// The release gaining traffic.
    pub candidate: String,
    /// The release that served when the rollout began.
    pub baseline: String,
    /// The steps, in order; the last one passing promotes the candidate.
    pub steps: Vec<RolloutStep>,
    /// The gates.
    pub gates: RolloutGates,
    /// Whether each step after the first waits for approval.
    pub approve_each_step: bool,
    /// Where it is.
    pub state: RolloutState,
    /// The current step's index.
    pub step: usize,
    /// When the current step began, Unix milliseconds.
    pub step_started_unix_ms: u64,
    /// What it is doing, or why it ended.
    pub message: String,
    /// When it was created, Unix milliseconds.
    pub created_unix_ms: u64,
    /// When it last changed, Unix milliseconds.
    pub updated_unix_ms: u64,
}

impl Rollout {
    /// The candidate's share in force: the current step's while running
    /// or paused, the step before while the next awaits approval, all of
    /// it once promoted and none once rolled back.
    pub fn percent(&self) -> u32 {
        match self.state {
            RolloutState::Promoted => 100,
            RolloutState::RolledBack => 0,
            RolloutState::AwaitingApproval => self
                .step
                .checked_sub(1)
                .and_then(|index| self.steps.get(index))
                .map_or(0, |step| step.percent),
            RolloutState::Running | RolloutState::Paused => {
                self.steps.get(self.step).map_or(0, |step| step.percent)
            }
        }
    }
}

/// A release record: its spec, where it is in section 9.1, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The application.
    pub app: String,
    /// The release id, unique within the application.
    pub id: String,
    /// The spec.
    pub spec: ReleaseSpec,
    /// Where it is.
    pub state: ReleaseState,
    /// Why it failed, or what it is doing.
    pub message: String,
    /// When it was created, Unix milliseconds.
    pub created_unix_ms: u64,
    /// For optimistic concurrency.
    pub resource_version: u64,
}

/// An application: a name and which of its releases serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// The name.
    pub name: String,
    /// The release traffic goes to, if any.
    pub current: Option<String>,
    /// The release a rollback restores, if any.
    pub previous: Option<String>,
    /// When it was created, Unix milliseconds.
    pub created_unix_ms: u64,
    /// For optimistic concurrency.
    pub resource_version: u64,
    /// The user its hosts and workers run as, when the daemon is root
    /// (section 10.9).
    pub user: Option<String>,
    /// The group, else the user's primary group.
    pub group: Option<String>,
    /// The file creation mask, in octal.
    pub umask: Option<String>,
    /// Which applications' scale-ups the node's budget admits first, the
    /// highest first (section 7.4).
    pub priority: i32,
}

impl App {
    /// The identity as one phrase, or none.
    pub fn describe_identity(&self) -> Option<String> {
        let user = self.user.as_ref()?;
        let mut text = user.clone();

        if let Some(group) = &self.group {
            text.push(':');
            text.push_str(group);
        }

        if let Some(umask) = &self.umask {
            text.push_str(&format!(" umask {umask}"));
        }

        Some(text)
    }
}

/// Whether a string is an octal file creation mask such as 027.
pub fn valid_umask(text: &str) -> bool {
    !text.is_empty() && text.len() <= 4 && text.chars().all(|c| ('0'..='7').contains(&c))
}

/// One state change (section 11.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The node-wide sequence number.
    pub sequence: u64,
    /// When, Unix milliseconds.
    pub time_unix_ms: u64,
    /// For example release.ready.
    pub kind: String,
    /// What it is about: app, or app@release.
    pub subject: String,
    /// info, notice, warning or error.
    pub level: &'static str,
    /// The sentence.
    pub message: String,
}

/// An overlay's state (section 9.10): active, with what made it so, or
/// held inactive after a person ended it, its preconditions failed or it
/// was promoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayState {
    /// The overlay.
    pub name: String,
    /// Whether it is applied.
    pub active: bool,
    /// `schedule` or `manual` while active; `ended`, `skipped` or
    /// `promoted` while held.
    pub source: String,
    /// When it became active or was held, Unix milliseconds.
    pub since_unix_ms: u64,
    /// When it ends, while active, or may activate again, while held;
    /// never without one.
    pub until_unix_ms: Option<u64>,
}

/// Check a name against section 3.2: lowercase letters, digits and
/// hyphens, up to 63 characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Whether a string is an environment variable name: letters, digits and
/// underscores, not starting with a digit.
pub fn valid_env_name(name: &str) -> bool {
    let mut characters = name.chars();

    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }

    characters.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Now, as Unix milliseconds.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_through_their_names() {
        for state in ReleaseState::ALL {
            assert_eq!(state.as_str().parse::<ReleaseState>(), Ok(state));
        }

        assert!("running".parse::<ReleaseState>().is_err());
    }

    #[test]
    fn names_follow_the_rules() {
        assert!(valid_name("shop"));
        assert!(valid_name("shop-2"));
        assert!(!valid_name("Shop"));
        assert!(!valid_name("-shop"));
        assert!(!valid_name(""));
        assert!(!valid_name("a".repeat(64).as_str()));
    }
}
