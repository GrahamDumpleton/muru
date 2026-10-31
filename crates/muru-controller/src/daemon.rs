//! The daemon: one long-running process that holds desired state, drives
//! releases toward it and serves the admin API (design specification
//! sections 9.1, 9.5, 10.1 and 13.3).
//!
//! This is the first slice: one node, one edge process (ADR 0002) on a
//! listener the daemon binds and hands down, applications deployed from
//! source directories with the blue/green strategy (start the new release,
//! move traffic when it is ready, drain the previous one), rollback to the
//! previous release, drains and events. Re-adoption of hosts across daemon
//! restarts, retention of drained releases and the rest of phase 2 follow.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use muru_proto::edge::v1 as pb;
use tokio::sync::{mpsc, oneshot};

use crate::edges::{EdgeProcess, EdgeSpec};
use crate::events::Events;
use crate::host::{HostControl, HostMessage, WorkerReply};
use crate::host::{HostProcess, HostSpec};
use crate::locations::Locations;
use crate::logs::Logs;
use crate::model::{
    Release, ReleaseSpec, ReleaseState, Rollout, RolloutGates, RolloutState, RolloutStep,
    now_unix_ms, valid_name,
};
use crate::rollouts::{Counters, Verdict, judge};
use crate::scaling::{Decision, ScaleStatus, Scaler, Signals};
use crate::servers::{Servers, ServersRecord};
use crate::store::Store;

/// How long a release may take from its host starting to every worker
/// listening (section 9.1 startup timeout, provisional).
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// How long requests in flight on a drained release may take to finish
/// before its workers are stopped anyway (section 9.5 drain_timeout).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a superseded release stays warm (section 9.1), provisional
/// (section 19.2).
pub const RETENTION: Duration = Duration::from_secs(30 * 60);

/// The workers each group of a retained release keeps, its warm floor
/// (section 9.1).
pub const RETAIN_WORKERS: u32 = 1;

/// How long to wait for a release's processes to stop, given its stop
/// budget: never less than the drain timeout.
fn stop_wait(budget: Duration) -> Duration {
    DRAIN_TIMEOUT.max(crate::host::stop_wait(budget))
}

/// The open WebSockets the dashboard lists, the longest open first.
const DASHBOARD_SOCKETS: u32 = 200;

/// Section 9.1: how long a superseded release stays warm, and the
/// workers each of its groups keeps meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// How long; zero stops it once it has drained.
    pub lasts: Duration,
    /// The workers each group keeps.
    pub workers: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            lasts: RETENTION,
            workers: RETAIN_WORKERS,
        }
    }
}

/// Section 9.4: how long sticky clients keep a release that is leaving
/// traffic, unless the policy says otherwise.
pub const STICKY_GRACE: Duration = Duration::from_secs(10 * 60);

/// The account a root daemon runs the edge and the applications as when
/// none is named (ADR 0026).
pub const SERVICE_USER: &str = "muru";

/// What the daemon runs with.
pub struct Config {
    /// Where everything lives.
    pub locations: Locations,
    /// The configuration file's settings, read as a reload reads them.
    pub settings: crate::settings::NodeSettings,
    /// The command line's settings, laid over the file on every reload.
    pub overrides: crate::settings::Overrides,
    /// Where the edge writes the access log, or none.
    pub access_log: Option<PathBuf>,
    /// The executable to spawn edges from, normally the daemon's own.
    pub executable: PathBuf,
    /// The arguments to re-execute the daemon with on an upgrade (section
    /// 13.4): its own, with the flag that names the version it ran before.
    pub upgrade_arguments: Vec<std::ffi::OsString>,
    /// The version this daemon ran before it re-executed itself for an
    /// upgrade, or none when it started afresh.
    pub upgraded_from: Option<String>,
}

/// What reading the TLS hosts' certificate files again found (section
/// 4.2).
#[derive(Debug, Clone, Default)]
pub struct CertificateChanges {
    /// Hosts whose certificate or key changed, which the edge is given.
    pub renewed: Vec<String>,
    /// Hosts kept on the certificate they had, with why the files were
    /// refused.
    pub kept: Vec<(String, String)>,
}

/// An upgrade the daemon is about to carry out (section 13.4).
#[derive(Debug, Clone)]
pub struct Upgrade {
    /// The installed binary the daemon re-executes from.
    pub executable: PathBuf,
    /// The version that binary reports.
    pub version: String,
}

/// What to do with an active rollout (section 9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutAction {
    /// Hold the current step until resumed.
    Pause,
    /// Continue: from a pause, or into a step awaiting approval.
    Resume,
    /// Give the candidate all the traffic now.
    Promote,
    /// Take the candidate out of traffic now.
    Abort,
}

/// Section 9.3: each release's failed probes, by application and
/// release, and how the latest failed.
type ProbeFailures = Arc<std::sync::Mutex<HashMap<(String, String), (u64, String)>>>;

/// Where a rollout's current step started from: the releases' counters
/// at that moment, so the gates judge the step alone.
#[derive(Debug, Clone)]
struct RolloutRun {
    rollout: String,
    step: usize,
    candidate: Counters,
    baseline: Counters,
}

/// Section 7.3: the shortest time the workers' signals are measured
/// over.
const SIGNAL_WINDOW: Duration = Duration::from_secs(4);

/// A request for the reconciler, answered when it has been carried out.
pub enum Command {
    /// Everything the dashboard shows (section 10.7).
    Dashboard {
        /// Answered with the data.
        done: oneshot::Sender<muru_dashboard::NodeData>,
    },
    /// The node's metrics as the Prometheus text exposition.
    Metrics {
        /// Answered with the text.
        done: oneshot::Sender<String>,
    },
    /// The WebSockets open on the edge.
    WebSockets {
        /// Only this application's; all when empty.
        app: String,
        /// How many to list, the longest open first.
        limit: u32,
        /// Answered with the edge's report.
        done: oneshot::Sender<anyhow::Result<pb::WebSockets>>,
    },
    /// Create a release and apply the blue/green strategy.
    Deploy {
        /// The application.
        app: String,
        /// The spec, resolved by the API from the request and the source,
        /// boxed to keep the command small.
        spec: Box<ReleaseSpec>,
        /// The release id, or none to generate one.
        id: Option<String>,
        /// How long the superseded release stays warm, zero to stop it,
        /// and the workers it keeps.
        retain: Retention,
        /// Start the release but leave traffic where it is (section 9.2).
        hold: bool,
        /// Where the release record goes as soon as it exists.
        created: oneshot::Sender<Result<Release, String>>,
        /// Where the outcome goes once the release serves or has failed.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Restore traffic to the application's previous release.
    Rollback {
        /// The application.
        app: String,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Section 9.3: start a rollout of a release against the one serving.
    StartRollout {
        /// The application.
        app: String,
        /// The candidate release.
        release: String,
        /// The steps, in order.
        steps: Vec<RolloutStep>,
        /// The gates.
        gates: RolloutGates,
        /// Whether each step after the first waits for approval.
        approve_each_step: bool,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Rollout, String>>,
    },
    /// Pause, resume, promote or abort the application's active rollout.
    ControlRollout {
        /// The application.
        app: String,
        /// What to do.
        action: RolloutAction,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Rollout, String>>,
    },
    /// Section 7.4: pin a worker group's count, or let the autoscaler
    /// decide again.
    SetScale {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The worker group.
        group: String,
        /// The count to pin.
        processes: Option<u32>,
        /// Let the autoscaler decide again.
        unpin: bool,
        /// Where the groups' states go.
        done: oneshot::Sender<Result<Vec<(String, ScaleStatus)>, String>>,
    },
    /// The autoscaler's state for each group of a release.
    GetScaleStatus {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// Where the groups' states go.
        done: oneshot::Sender<Result<Vec<(String, ScaleStatus)>, String>>,
    },
    /// Section 4.12: serve the maintenance page for an application, or
    /// serve it as usual again.
    SetMaintenance {
        /// The application.
        app: String,
        /// On or off.
        on: bool,
        /// Answered with when maintenance began, or none when it is off.
        done: oneshot::Sender<Result<Option<u64>, String>>,
    },
    /// Blue/green (section 9.2): move all of an application's traffic to a
    /// release now and drain the one that served.
    PromoteRelease {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Take a release out of traffic and stop it.
    Drain {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Push a fresh snapshot: the hosts or routes changed.
    Reconfigure {
        /// Where the acknowledgement goes.
        done: oneshot::Sender<Result<(), String>>,
    },
    /// Section 4.2: read the TLS hosts' certificate files again, for an
    /// apply that changed nothing else, and send the edge what changed.
    RefreshCertificates {
        /// Answered with what was renewed and what was kept.
        done: oneshot::Sender<CertificateChanges>,
    },
    /// Re-read the configuration file for what can change while running
    /// (section 13.3).
    Reload {
        /// Answered with what was reloaded, or why it was refused.
        done: oneshot::Sender<Result<String, String>>,
    },
    /// Reopen every log file after an external rotation (section 11.3):
    /// the daemon's own, the edge's access log and each host's, and with
    /// `workers` the application's own files through on_reopen_logs.
    ReopenLogs {
        /// Also pass the request on to the workers.
        workers: bool,
        /// Where the count of what was told goes.
        done: oneshot::Sender<Reopened>,
    },
    /// Stop the daemon: detaching, with hosts and the edge left serving
    /// for the next daemon to re-adopt, or draining everything first.
    Shutdown {
        /// Drain and stop every release and the edge before exiting.
        drain: bool,
        /// Where the acknowledgement goes, before the daemon exits.
        done: oneshot::Sender<()>,
    },
    /// Re-execute the daemon from the installed binary and replace the
    /// edge (section 13.4).
    Upgrade {
        /// Answered with what is about to happen, before the daemon
        /// re-executes, or why it will not.
        done: oneshot::Sender<Result<Upgrade, String>>,
    },
    /// A host's control connection is up (section 5.9).
    HostAttached {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The connection, for acknowledgements.
        control: HostControl,
    },
    /// A host said something over its control connection.
    HostMessage {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// What it said.
        message: HostMessage,
    },
    /// Ask a release's workers something (section 11.7).
    Debug {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The operation: status, stacks, tasks, inspect or logging.
        op: String,
        /// One worker, or every worker.
        worker: Option<u32>,
        /// The operation's arguments.
        args: serde_json::Value,
        /// Answered with each worker's reply, or why none could be asked.
        done: oneshot::Sender<Result<Vec<WorkerReply>, String>>,
    },
    /// Activate, end or promote an overlay (section 9.10).
    Overlay {
        /// The overlay.
        name: String,
        /// What to do.
        action: OverlayAction,
        /// Whether the signed webhook asks, rather than a person.
        external: bool,
        /// Answered with a sentence about what happened.
        done: oneshot::Sender<Result<String, String>>,
    },
    /// Section 7.5: a temporary schedule raising a worker group's floor,
    /// and its ceiling with it, until a time.
    ScaleFloor {
        /// The application.
        app: String,
        /// The worker group.
        group: String,
        /// The floor.
        min_workers: u32,
        /// The ceiling, when raised too.
        max_workers: Option<u32>,
        /// When it ends, Unix milliseconds.
        until: u64,
        /// Answered with a sentence about what happened.
        done: oneshot::Sender<Result<String, String>>,
    },
}

/// What a person asks of an overlay (section 9.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAction {
    /// Apply it now, until a time or until ended.
    Activate {
        /// When it ends, Unix milliseconds; never without one.
        until: Option<u64>,
    },
    /// Stop applying it; a schedule's window stays ended until it is over.
    End,
    /// Fold its changes into the base configuration and end it for good.
    Promote,
}

/// What a previous daemon left behind about its edge, for re-adoption
/// (section 10.2), in the runtime directory.
#[derive(serde::Serialize, serde::Deserialize)]
struct EdgeRecord {
    pid: u32,
    control: PathBuf,
    listener_socket: PathBuf,
    #[serde(default)]
    generation: u32,
    #[serde(default)]
    listeners: Vec<ListenerRecord>,
}

/// One of the edge's listeners, for re-adoption.
#[derive(serde::Serialize, serde::Deserialize)]
struct ListenerRecord {
    address: SocketAddr,
    tls: bool,
}

/// What a previous daemon left behind about a release's host, in the
/// release's run directory.
#[derive(serde::Serialize, serde::Deserialize)]
struct HostRecord {
    pid: u32,
    sockets: Vec<PathBuf>,
    #[serde(default)]
    groups: Vec<GroupRecord>,
}

/// A named worker group's sockets and paths, for re-adoption and the
/// snapshot.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct GroupRecord {
    name: String,
    paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    client_classes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    methods: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
    sockets: Vec<PathBuf>,
}

/// Section 7.4: one worker group's autoscaler and what was last asked of
/// the host for it.
struct GroupScaling {
    name: String,
    scaler: Scaler,
    /// Slots a worker offers before any is routed to: its threads, or
    /// its event loop's concurrency.
    slots_per_worker: f64,
    /// The count last asked of the host, so a count the host has not
    /// reached yet is not asked for again every second.
    requested: Option<u32>,
    /// The threads a worker can run Python on at once without a GIL,
    /// for its CPU share (section 7.3).
    threads: u32,
    /// The soft memory limit and what it measures, when one is set.
    memory_limit: Option<(muru_config::Measure, u64)>,
    /// The last scale-out the node's budget held back: the count asked
    /// for, the count given and the limit, so each is one event.
    denied: Option<(u32, u32, crate::budget::Limit)>,
}

/// What the API needs to talk to a running daemon.
#[derive(Clone)]
pub struct Handle {
    /// The store, for reads.
    pub store: Arc<Store>,
    /// The event stream.
    pub events: Events,
    /// Where commands go.
    pub commands: mpsc::Sender<Command>,
    /// The daemon's locations and listener, for status.
    pub info: Arc<Info>,
}

/// Facts about the running daemon, for status.
pub struct Info {
    /// Where everything lives.
    pub locations: Locations,
    /// The addresses the edge serves, each with whether it carries TLS,
    /// as a reload may change them.
    pub listeners: std::sync::RwLock<Vec<(SocketAddr, bool)>>,
    /// When the daemon started, Unix milliseconds.
    pub started_unix_ms: u64,
    /// The edge process's pid, for status.
    pub edge_pid: std::sync::atomic::AtomicU32,
    /// Where the Prometheus exposition is served, or nowhere.
    pub metrics: Option<SocketAddr>,
    /// Where the dashboard is served, or nowhere.
    pub dashboard: Option<SocketAddr>,
    /// The dashboard's login codes and sessions, when it is served
    /// (section 10.7).
    pub dashboard_sessions: Option<Arc<muru_dashboard::Sessions>>,
    /// Whether a dashboard session may be given the actions, which
    /// `muructl dashboard login --actions` asks for, and whether a session
    /// that has them may act: `[dashboard] actions`, re-read on reload.
    pub dashboard_actions: Arc<std::sync::atomic::AtomicBool>,
    /// The rate limits (section 4.15) and client classes (section 4.14)
    /// the daemon's file defines, which applied files may name.
    pub defined: std::sync::RwLock<crate::config::Defined>,
    /// WebSockets open by application and release, from the edge's
    /// metrics as last sampled.
    pub websockets: std::sync::Mutex<HashMap<(String, String), u64>>,
    /// What each release's entrypoints were detected as, by application
    /// and release, then by mount, the default mount's under the empty
    /// name: as its host last announced them.
    pub interfaces: std::sync::Mutex<Interfaces>,
    /// Section 8.4: uv as found and how it is run, from the `[tools]`
    /// table read at start.
    pub provisioning: crate::provision::Provisioning,
}

/// Detected interfaces by application and release, then by mount.
pub type Interfaces = HashMap<(String, String), Vec<(String, String)>>;

/// What a ReopenLogs command reached.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reopened {
    /// Whether the edge was told.
    pub edge: bool,
    /// How many hosts were told.
    pub hosts: u32,
}

/// What runs a release: a host with Muru's workers, the instances of a
/// managed HTTP server (section 9.9), or nothing for now: a release whose
/// host stopped once every group had been at zero for its
/// host_idle_timeout, started again by the next request (section 7.4).
enum Backend {
    Host(HostProcess),
    Servers(Servers),
    Dormant,
}

impl Backend {
    /// The host, for a release with workers.
    fn host(&self) -> Option<&HostProcess> {
        match self {
            Self::Host(host) => Some(host),
            Self::Servers(_) | Self::Dormant => None,
        }
    }

    /// The managed server's instances, for a release that runs one.
    fn servers(&self) -> Option<&Servers> {
        match self {
            Self::Servers(servers) => Some(servers),
            Self::Host(_) | Self::Dormant => None,
        }
    }

    /// Whether the host has exited on its own: a sentence saying how.
    /// Instances look after themselves (check_hosts starts them again).
    fn try_exited(&mut self) -> std::io::Result<Option<String>> {
        match self {
            Self::Host(host) => host.try_exited(),
            Self::Servers(_) | Self::Dormant => Ok(None),
        }
    }

    /// The record a previous daemon reads to take the release over.
    fn record_path(&self, run_dir: &Path) -> PathBuf {
        match self {
            Self::Host(_) | Self::Dormant => run_dir.join("host.toml"),
            Self::Servers(_) => run_dir.join("servers.toml"),
        }
    }

    /// Stop the processes; `grace` is the release's stop budget: what a
    /// managed server's instances, or a host's workers, get before
    /// SIGKILL.
    async fn stop(self, grace: Duration) {
        match self {
            Self::Host(host) => host.stop(grace).await,
            Self::Servers(servers) => servers.stop(grace).await,
            Self::Dormant => {}
        }
    }

    /// Stop the processes as `stop` does, killing what is still there at
    /// `kill_at` (section 13.3).
    async fn stop_until(self, kill_at: Instant) {
        match self {
            Self::Host(host) => host.stop_until(kill_at).await,
            Self::Servers(servers) => servers.stop_until(kill_at).await,
            Self::Dormant => {}
        }
    }

    /// Leave the processes running for the next daemon.
    fn release(self) {
        match self {
            Self::Host(host) => host.release(),
            Self::Servers(servers) => servers.release(),
            Self::Dormant => {}
        }
    }

    /// Send the host a signal; a managed server's instances take none,
    /// since the daemon writes their log lines itself.
    fn signal(&self, signal: i32) -> bool {
        match self {
            Self::Host(host) => host.signal(signal),
            Self::Servers(_) | Self::Dormant => false,
        }
    }

    /// Section 9.9: a managed server's instances as the upstream that
    /// stands in for the release's workers in the snapshot.
    fn server_message(&self, app: &str, id: &str) -> Option<pb::Upstream> {
        self.servers().map(|servers| pb::Upstream {
            name: format!("{app}@{id}"),
            addresses: servers.addresses(),
            protocol: "http1".to_owned(),
            trust_muru_headers: true,
            eject_after: 3,
            eject_for_ms: 5_000,
            ..Default::default()
        })
    }
}

/// A running release: its host and the sockets its workers listen on,
/// or the instances of its managed server, and, once superseded, when
/// its retention ends.
struct Running {
    backend: Backend,
    /// The default group's sockets.
    sockets: Vec<PathBuf>,
    /// The named groups' sockets and paths.
    groups: Vec<GroupRecord>,
    retained_until: Option<Instant>,
    /// Section 9.4: while set, the release is leaving traffic but stays
    /// reachable by sticky cookie until the instant, when it drains with
    /// the retention given.
    sticky_until: Option<(Instant, Retention)>,
    /// Deployed with hold (section 9.2): running and reachable by preview,
    /// taking no traffic until a rollout or a promotion, and left alone
    /// by the traffic reconciliation.
    held: bool,
    /// The host's control connection, once it is up.
    control: Option<HostControl>,
    /// What the workers last reported, for metrics.
    status: Vec<crate::host::WorkerStatus>,
    /// Section 11.1: workers the host replaced, by group and reason, and
    /// workers that exited, likewise, since the release started.
    recycles: BTreeMap<(String, String), u64>,
    exits: BTreeMap<(String, String), u64>,
    /// Section 7.3: the report the workers' signals are measured from,
    /// and when it arrived.
    signals_from: Option<(Instant, Vec<crate::host::WorkerStatus>)>,
    /// Section 7.4: each worker group's autoscaler, the default first.
    scalers: Vec<GroupScaling>,
    /// The host's own proportional set size, where it can be read, for
    /// the node's memory budget.
    host_pss: Option<u64>,
    /// Section 7.4: since when every group has been at zero, for the
    /// host_idle_timeout.
    idle_since: Option<Instant>,
    /// When each worker socket entered routing, for the age of the
    /// youngest worker.
    ready_at: HashMap<PathBuf, Instant>,
}

impl Running {
    /// Section 7.4: what one worker of a group costs, from its workers'
    /// mean proportional set size and CPU, the soft memory limit standing
    /// in before any has reported, then `fallback`.
    fn worker_cost(&self, group: &GroupScaling, fallback: Option<u64>) -> crate::budget::Cost {
        let sizes: Vec<u64> = self
            .status
            .iter()
            .filter(|worker| worker.group == group.name)
            .filter_map(|worker| worker.pss)
            .collect();
        let measured = (!sizes.is_empty()).then(|| sizes.iter().sum::<u64>() / sizes.len() as u64);

        crate::budget::Cost {
            memory: measured
                .or(group.memory_limit.map(|(_, limit)| limit))
                .or(fallback),
            cpu: group.scaler.worker_signals().cores,
        }
    }

    /// Every socket the edge routes to: the default group's, then the
    /// named groups'.
    fn routed_sockets(&self) -> Vec<PathBuf> {
        let mut all = self.sockets.clone();

        for group in &self.groups {
            all.extend(group.sockets.iter().cloned());
        }

        all
    }

    /// A group's workers, as the sockets the host announced; none for a
    /// group it has not announced yet.
    fn group_sockets(&self, group: &str) -> Option<&[PathBuf]> {
        if group == muru_edge::routing::DEFAULT_GROUP {
            return Some(&self.sockets);
        }

        self.groups
            .iter()
            .find(|record| record.name == group)
            .map(|record| record.sockets.as_slice())
    }

    /// Section 7.4: the groups with no workers, whose requests wait at
    /// the edge to be woken rather than being refused: a group scaled to
    /// zero, or one whose workers are on their way.
    fn waking_groups(&self) -> Vec<String> {
        if self.backend.servers().is_some() {
            return Vec::new();
        }

        std::iter::once((muru_edge::routing::DEFAULT_GROUP, &self.sockets))
            .chain(
                self.groups
                    .iter()
                    .map(|group| (group.name.as_str(), &group.sockets)),
            )
            .filter(|(_, sockets)| sockets.is_empty())
            .map(|(name, _)| name.to_owned())
            .collect()
    }

    /// Section 7.4: what the release holds against the node's budget:
    /// each group at the larger of the workers it has and the count its
    /// autoscaler is bringing it to, at what a worker costs, and the
    /// host's own memory; a managed server's instances count as workers.
    fn usage(&self, fallback: Option<u64>) -> crate::budget::Usage {
        let mut usage = crate::budget::Usage::default();

        if matches!(self.backend, Backend::Servers(_)) {
            usage.workers = self.workers() as u32;
            return usage;
        }

        for group in &self.scalers {
            let mut count = self.group_sockets(&group.name).map_or(0, <[PathBuf]>::len) as u32;

            if self.retained_until.is_none() {
                count = count.max(group.scaler.desired());
            }

            usage.add(count, self.worker_cost(group, fallback));
        }

        usage.memory += self.host_pss.unwrap_or(0);
        usage
    }

    /// The processes serving: workers in every group, or a managed
    /// server's instances.
    fn workers(&self) -> usize {
        match &self.backend {
            Backend::Servers(servers) => servers.running(),
            Backend::Host(_) | Backend::Dormant => {
                self.sockets.len()
                    + self
                        .groups
                        .iter()
                        .map(|group| group.sockets.len())
                        .sum::<usize>()
            }
        }
    }
}

/// The daemon's state while it runs.
struct Daemon {
    store: Arc<Store>,
    events: Events,
    locations: Locations,
    edge: EdgeProcess,
    info: Arc<Info>,
    logs: Logs,
    /// The [proxy] table of the configuration file, for every snapshot.
    proxy: Option<pb::Proxy>,
    /// The [compression] table, for every snapshot.
    compression: pb::Compression,
    /// The node-wide [error_pages] and [error_override] tables (section
    /// 4.12), for every snapshot.
    error_pages: Vec<pb::ErrorPage>,
    error_override: Option<pb::ErrorOverride>,
    /// The daemon file's client classes and rate limits (sections 4.14
    /// and 4.15).
    client_rules: crate::clients::FileRules,
    /// Those with the applied files', for every snapshot, with the list
    /// files they were read from.
    clients: crate::clients::ClientPolicies,
    /// Which rate limits are refusing requests, for the events.
    enforcement: crate::clients::Enforcement,
    /// The applications in maintenance mode (section 4.12), with when
    /// each was put there.
    maintenance: HashMap<String, u64>,
    /// Section 9.10: the overlays in the warm-up lead before a window,
    /// with the release each keeps running meanwhile.
    warm_ups: BTreeMap<String, Option<(String, String)>>,
    /// Section 9.10: when the clock's synchronisation was last checked,
    /// and whether the daemon has warned that it is not synchronised.
    clock: (Option<Instant>, bool),
    /// The secret stores releases may refer to (section 10.8).
    secrets: Vec<crate::secrets::Store>,
    /// Built-in log rotation (section 11.3), when it is on.
    rotation: Option<crate::rotation::Rotator>,
    /// Section 13.3: what SIGTERM means, and how a shutdown runs.
    shutdown: crate::shutdown::Policy,
    /// The service user (ADR 0026), when this daemon is root: whom the
    /// edge and the applications that name no user run as.
    service: Option<crate::host::Identity>,
    /// Whether `muructl debug exec` may run code in workers.
    debug_exec: bool,
    /// The listeners' h2c policies, for every snapshot (section 6.13).
    h2c: Vec<pb::H2cPolicy>,
    /// The `[limits]` table (section 4.6), for every snapshot.
    limits: muru_edge::Limits,
    /// The command line's settings, laid over the file on every reload.
    overrides: crate::settings::Overrides,
    /// The event sinks delivering (section 11.5), reconciled on reload.
    sinks: crate::sinks::Sinks,
    /// Section 4.2: each TLS host's last good certificate chain and key
    /// as PEM text, by host, for the snapshots; read again on reload and
    /// apply, and kept when the files are wrong.
    certificates: HashMap<String, (String, String)>,
    /// Section 7.4: the node's budget, and the schedule windows already
    /// checked against it, by overlay and start.
    budget: crate::budget::Budget,
    budget_checked: BTreeSet<(String, u64)>,
    /// The points the dashboard's charts draw, one a second.
    history: Arc<muru_dashboard::History>,
    /// Turns the edge's counters into those points.
    sampler: muru_dashboard::Sampler,
    running: HashMap<(String, String), Running>,
    /// Section 9.3: the counters each running rollout's current step
    /// started from, by application.
    rollout_runs: HashMap<String, RolloutRun>,
    /// Section 9.3: each release's worker crashes and timeout kills, from
    /// the host's events, for the gates.
    worker_faults: HashMap<(String, String), u64>,
    /// Section 9.3: each release's failed probes and how the latest
    /// failed, written by the probe tasks.
    probe_failures: ProbeFailures,
    /// When each application's rollout probes were last sent.
    probed: HashMap<String, Instant>,
    /// The client probes are sent with.
    probe_client: crate::rollouts::ProbeClient,
    /// The release each application's traffic goes to, as the edge has
    /// been told; the store catches up once a switch has happened.
    serving: HashMap<String, String>,
    /// The application whose workers take requests when no host is
    /// declared: the last one traffic was switched to.
    default_app: Option<String>,
    /// Whether this process writes to the daemon log file, as a daemon
    /// started in the background does, so a reopen applies to it too.
    owns_log: bool,
    /// Where the hosts' control messages are sent, into the same inbox
    /// as the commands.
    commands: mpsc::Sender<Command>,
    /// Requests to hosts awaiting their answers, by request id.
    asked: HashMap<u64, oneshot::Sender<Result<Vec<WorkerReply>, String>>>,
    /// The last request id given out.
    last_request: u64,
}

/// Run the daemon until it is told to stop: bind the edge, serve the admin
/// API, start the releases that were serving, then reconcile commands as
/// they arrive.
pub async fn run(config: Config) -> anyhow::Result<()> {
    config
        .locations
        .check_run_dir()
        .map_err(|error| anyhow::anyhow!(error))?;

    // The file was checked whole when it was read (section 10.3); a
    // reload reads it the same way and applies the difference.

    let mut settings = config.settings;
    let proxy = settings.proxy.clone();
    let compression = settings.compression.clone();
    let error_pages = settings.error_pages.clone();
    let error_override = settings.error_override.clone();
    let client_rules = settings.client_rules.clone();
    let metrics = settings.metrics;
    let dashboard = settings.dashboard;
    let dashboard_sessions = dashboard.is_some().then(muru_dashboard::Sessions::new);
    let dashboard_actions = Arc::new(std::sync::atomic::AtomicBool::new(
        settings.dashboard_actions,
    ));
    let sinks = std::mem::take(&mut settings.sinks);
    let locations = config.locations;

    for location in [&locations.state, &locations.run, &locations.logs] {
        std::fs::create_dir_all(&location.path)
            .with_context(|| format!("creating {}", location.path.display()))?;
    }

    // ADR 0026: a daemon that is root starts everything else as the
    // service user, and does not start without one.

    let service = service_identity(settings.service_user.as_deref())?;

    if let (Some(service), Some(access_log)) = (&service, &config.access_log) {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(access_log);
        service
            .own(access_log)
            .with_context(|| format!("giving {} to the service user", access_log.display()))?;
    }

    let store = Arc::new(Store::open(&locations.state_store())?);
    let clients = client_rules
        .policies(&store)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let events = Events::new(Arc::clone(&store));

    // Section 11.5: each sink delivers from its own cursor, so nothing
    // emitted from here on is missed by one.

    let sinks = crate::sinks::Sinks::start(sinks, Arc::clone(&store), events.clone());

    // An edge a previous daemon left running is taken over, with its
    // listener handed back. Otherwise the daemon binds the listener and
    // keeps its own copy, so the edge process can be replaced while
    // connections wait in the backlog; the edge is up before any release,
    // answering 503 until one serves.

    let edge_log = locations.logs.path.join("edge.log");

    // Section 4.11: the upload spool is the edge's own, in the state
    // directory rather than the run directory, which may be memory.

    let spool = locations.state.path.join("spool");
    prepare_spool(&spool, service)
        .with_context(|| format!("preparing the upload spool {}", spool.display()))?;

    let adopted = match read_record::<EdgeRecord>(&locations.run.path.join("edge.toml")) {
        Some(record) => EdgeProcess::adopt(
            record.pid,
            config.executable.clone(),
            record.control,
            record.listener_socket,
            record.generation,
            record
                .listeners
                .iter()
                .map(|listener| listener.tls)
                .collect(),
            edge_log.clone(),
            config.access_log.clone(),
            spool.clone(),
            service,
        )
        .await
        .map_err(|error| tracing::info!(%error, "no edge to re-adopt"))
        .ok(),
        None => None,
    };

    let edge = match adopted {
        Some(edge) => {
            tracing::info!(pid = edge.pid().unwrap_or(0), "re-adopted the edge");
            edge
        }
        None => {
            anyhow::ensure!(!settings.listeners.is_empty(), "no listener to serve on");
            let mut listeners = Vec::new();

            for (address, tls) in &settings.listeners {
                let socket = std::net::TcpListener::bind(address)
                    .with_context(|| format!("cannot listen on {address}"))?;
                listeners.push(crate::edges::Listener { socket, tls: *tls });
            }

            let (control, listener_socket) =
                crate::edges::socket_paths(&crate::edges::directory(&locations.run.path), 1);
            EdgeProcess::spawn(EdgeSpec {
                executable: config.executable.clone(),
                listeners,
                control,
                listener_socket,
                generation: 1,
                log: edge_log,
                access_log: config.access_log,
                spool,
                identity: service,
            })
            .await?
        }
    };

    let listeners = edge.listeners();

    // Section 8.4: uv as found and how it is run, said once at start.

    let provisioning =
        crate::provision::Provisioning::for_daemon(&settings.tools, &locations.state.path);
    tracing::info!("{}", provisioning.describe_uv());
    write_edge_record(&locations.run.path, &edge);

    // The admin API on its socket.

    let (commands, mut inbox) = mpsc::channel::<Command>(64);
    let handle_commands = commands.clone();
    let info = Arc::new(Info {
        locations: locations.clone(),
        listeners: std::sync::RwLock::new(listeners.clone()),
        started_unix_ms: crate::model::now_unix_ms(),
        edge_pid: std::sync::atomic::AtomicU32::new(edge.pid().unwrap_or(0)),
        metrics,
        dashboard,
        dashboard_sessions: dashboard_sessions.clone(),
        dashboard_actions: Arc::clone(&dashboard_actions),
        defined: std::sync::RwLock::new(crate::config::Defined {
            budget: crate::budget::View {
                budget: settings.budget,
                ..crate::budget::View::default()
            },
            ..client_rules.defined()
        }),
        interfaces: std::sync::Mutex::new(Interfaces::new()),
        websockets: std::sync::Mutex::new(HashMap::new()),
        provisioning,
    });
    let handle = Handle {
        store: Arc::clone(&store),
        events: events.clone(),
        commands,
        info: Arc::clone(&info),
    };

    // The metrics listener (section 11.1), on an internal address.

    let scraper = match metrics {
        Some(address) => {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .with_context(|| format!("cannot listen for metrics on {address}"))?;
            Some(tokio::spawn(crate::metrics::serve(
                listener,
                handle.commands.clone(),
            )))
        }
        None => None,
    };

    // The dashboard and gateway (section 10.7), on an internal address,
    // with the history the reconciler records every second for its
    // charts.

    let history = Arc::new(muru_dashboard::History::new());

    let pages = match dashboard {
        Some(address) => {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .with_context(|| format!("cannot listen for the dashboard on {address}"))?;
            Some(tokio::spawn(crate::dashboard::serve(
                listener,
                handle.commands.clone(),
                Arc::clone(&history),
                events.clone(),
                dashboard_sessions.unwrap_or_else(muru_dashboard::Sessions::new),
                Arc::clone(&handle.store),
                Arc::clone(&dashboard_actions),
            )))
        }
        None => None,
    };

    // The overlays' external trigger (section 9.10), on its own address.

    let triggers = match settings.overlay_webhook.take() {
        Some(webhook) => {
            let listener = tokio::net::TcpListener::bind(webhook.address)
                .await
                .with_context(|| {
                    format!(
                        "cannot listen for the overlay webhook on {}",
                        webhook.address
                    )
                })?;
            Some(tokio::spawn(crate::triggers::serve(
                listener,
                webhook.secret,
                Arc::clone(&handle.store),
                handle.commands.clone(),
            )))
        }
        None => None,
    };

    let socket = locations.admin_socket();
    let (api_stop, api_stopped) = oneshot::channel();
    let api = crate::api::serve(socket.clone(), handle, api_stopped).await?;
    let mut api = tokio::spawn(api);
    let serving_on = listeners
        .iter()
        .map(|(address, tls)| format!("{}://{address}", if *tls { "https" } else { "http" }))
        .collect::<Vec<_>>()
        .join(" and ");

    match &config.upgraded_from {
        Some(before) => events.emit(
            "daemon.upgraded",
            "daemon",
            "info",
            &format!(
                "re-executed as version {} (was {before}) from {}, pid {}; serving on {serving_on}",
                env!("CARGO_PKG_VERSION"),
                config.executable.display(),
                std::process::id()
            ),
        ),
        None => events.emit(
            "daemon.started",
            "daemon",
            "info",
            &format!("serving on {serving_on}, admin socket {}", socket.display()),
        ),
    }

    let owns_log = writes_to(&locations.daemon_log());

    let mut daemon = Daemon {
        store,
        events,
        locations,
        edge,
        info,
        logs: Logs::default(),
        proxy,
        compression,
        error_pages,
        error_override,
        client_rules,
        clients,
        enforcement: crate::clients::Enforcement::default(),
        maintenance: HashMap::new(),
        warm_ups: BTreeMap::new(),
        clock: (None, false),
        secrets: settings.secrets.clone(),
        rotation: settings.rotation.clone().map(crate::rotation::Rotator::new),
        shutdown: settings.shutdown.clone(),
        service,
        debug_exec: settings.debug_exec,
        h2c: settings.h2c.clone(),
        limits: settings.limits.clone(),
        overrides: config.overrides,
        sinks,
        certificates: HashMap::new(),
        budget: settings.budget,
        budget_checked: BTreeSet::new(),
        history,
        sampler: muru_dashboard::Sampler::default(),
        running: HashMap::new(),
        rollout_runs: HashMap::new(),
        worker_faults: HashMap::new(),
        probe_failures: Arc::default(),
        probed: HashMap::new(),
        probe_client: crate::rollouts::probe_client(),
        serving: HashMap::new(),
        default_app: None,
        owns_log,
        commands: handle_commands,
        asked: HashMap::new(),
        last_request: 0,
    };
    daemon.restore().await;
    daemon.reconcile_traffic().await;

    // The edge has the node's settings, a listener's h2c among them, from
    // the start, whether or not any release runs yet.

    if let Err(error) = daemon.push_snapshot().await {
        tracing::warn!(%error, "pushing the first snapshot failed");
    }

    // Section 13.4: an upgraded daemon replaces the edge with one from
    // its own image once the hosts are re-adopted and the snapshot is
    // whole again.

    if config.upgraded_from.is_some() {
        let _ = daemon.replace_edge(None, "the same listeners").await;
    }

    // Section 7.4: the edge says when a request waits for a group with
    // no workers.

    let (wake_sender, mut wakes) = mpsc::channel(64);
    daemon.edge.watch_wakes(wake_sender);

    crate::notify::ready(&format!("serving on {serving_on}"));

    // Commands are carried out one at a time; a host dying in between is
    // noticed by the next pass. SIGTERM and SIGINT detach, as a service
    // manager's stop or a terminal's interrupt should, unless the
    // [shutdown] table or being a container's main process makes them
    // shut everything down (section 13.3), and SIGHUP reloads the
    // configuration file.

    let mut drain_on_exit = true;
    let mut upgrade_to: Option<Upgrade> = None;
    let mut reopen = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // The once-a-second work keeps its own clock, so a stream of commands
    // arriving faster than that never starves it.

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;

    let outcome = loop {
        tokio::select! {
            _ = reopen.recv() => {
                daemon.reopen_logs(false);
            }
            _ = hangup.recv() => {
                if let Err(error) = daemon.reload().await {
                    tracing::warn!(%error, "reloading the configuration failed");
                }
            }
            _ = terminate.recv() => {
                drain_on_exit = daemon.signalled("SIGTERM");
                break Ok(());
            }
            _ = interrupt.recv() => {
                drain_on_exit = daemon.signalled("SIGINT");
                break Ok(());
            }
            command = inbox.recv() => match command {
                Some(Command::Shutdown { drain, done }) => {
                    drain_on_exit = drain;
                    let _ = done.send(());
                    break Ok(());
                }
                Some(Command::Upgrade { done }) => match prepare_upgrade().await {
                    Ok(upgrade) => {
                        let _ = done.send(Ok(upgrade.clone()));
                        upgrade_to = Some(upgrade);
                        drain_on_exit = false;
                        break Ok(());
                    }
                    Err(error) => {
                        let _ = done.send(Err(error));
                    }
                },
                Some(command) => daemon.carry_out(command).await,
                None => break Ok(()),
            },
            Some(wake) = wakes.recv() => {
                daemon.wake(wake).await;
            }
            served = &mut api => {
                break served.map_err(anyhow::Error::from).and_then(|result| result);
            }
            _ = tick.tick() => {
                daemon.check_edge().await;
                daemon.check_hosts().await;
                daemon.expire_sticky().await;
                daemon.expire_retained().await;
                daemon.rotate_logs();

                let samples = match daemon.edge.metrics().await {
                    Ok(samples) => Some(samples),
                    Err(error) => {
                        tracing::debug!(%error, "the edge's metrics could not be read");
                        None
                    }
                };

                daemon.sample_history(samples.as_deref()).await;
                daemon.advance_rollouts(samples.as_deref()).await;
                daemon.autoscale(samples.as_deref()).await;
                daemon.refresh_client_lists().await;
                daemon.reconcile_overlays().await;
                daemon.check_clock();
                daemon.watch_rate_limits(samples.as_deref());
            }
        }
    };

    // Section 13.3: by default the daemon detaches, leaving hosts and the
    // edge serving for the next daemon to re-adopt; with drain it takes
    // everything down with it. For an upgrade (section 13.4) it detaches
    // the same way and then re-executes itself, so the next daemon is
    // this process with the installed binary's image.

    if upgrade_to.is_some() {
        crate::notify::notify("RELOADING=1\nSTATUS=upgrading");
    } else {
        crate::notify::stopping();
    }

    if drain_on_exit {
        // The shutdown, as the stop signal and muructl daemon stop
        // --shutdown both run it, inside the policy's budget: the delay
        // with the edge still accepting, the drain of the requests in
        // flight, every host stopped at once, and the edge last, so the
        // workers' WebSocket close frames get through it.

        let policy = daemon.shutdown.clone();
        let started = Instant::now();
        let deadline = started + policy.budget;
        daemon.drain_edge(&policy, deadline).await;
        daemon.stop_all_until(deadline).await;
        daemon
            .edge
            .stop_within(deadline.saturating_duration_since(Instant::now()))
            .await;
        let _ = std::fs::remove_file(daemon.locations.run.path.join("edge.toml"));
        daemon.events.emit(
            "daemon.stopped",
            "daemon",
            "info",
            &format!(
                "stopped in {:.1} s of the {} budget",
                started.elapsed().as_secs_f64(),
                muru_config::format_duration(policy.budget)
            ),
        );
    } else if upgrade_to.is_none() {
        daemon.detach();
        daemon.events.emit(
            "daemon.detached",
            "daemon",
            "info",
            "detached; hosts and the edge keep serving",
        );
    } else {
        daemon.detach();
    }

    // The API answers the Shutdown call that brought the daemon here
    // before the socket goes. A client holding a connection open would
    // keep the server from finishing, so after a grace period it is
    // aborted, which closes the listener and every connection before the
    // socket is unlinked; a stop command polling the socket then gets a
    // refusal straight away instead of waiting on a silent connection.

    let _ = api_stop.send(());

    if tokio::time::timeout(Duration::from_secs(5), &mut api)
        .await
        .is_err()
    {
        api.abort();
    }

    if let Some(scraper) = scraper {
        scraper.abort();
    }

    daemon.sinks.stop();

    if let Some(pages) = pages {
        pages.abort();
    }

    if let Some(triggers) = triggers {
        triggers.abort();
    }

    let _ = std::fs::remove_file(&socket);

    if let Some(upgrade) = upgrade_to {
        // exec returns only when it failed; the hosts and the edge are
        // still serving, so this daemon leaves as a detached one does.

        let error = exec_daemon(&upgrade.executable, &config.upgrade_arguments);
        daemon.events.emit(
            "daemon.detached",
            "daemon",
            "error",
            &format!(
                "re-executing {} failed: {error}; hosts and the edge keep serving",
                upgrade.executable.display()
            ),
        );
        return Err(anyhow::anyhow!(
            "re-executing {} failed: {error}",
            upgrade.executable.display()
        ));
    }

    outcome
}

/// Section 10.8: the environment a release's host starts with, its
/// secrets resolved from the stores; a secret delivered as a file is
/// written under `secrets_dir` and the variable NAME_FILE names it.
async fn resolve_env(
    stores: &[crate::secrets::Store],
    env: &[crate::model::EnvSpec],
    secrets_dir: &Path,
) -> Result<Vec<(String, String)>, String> {
    let mut resolved = Vec::with_capacity(env.len());

    for entry in env {
        let (Some(store_name), Some(key)) = (&entry.secret_store, &entry.secret_key) else {
            resolved.push((entry.name.clone(), entry.value.clone().unwrap_or_default()));
            continue;
        };
        let store = stores
            .iter()
            .find(|store| &store.name == store_name)
            .ok_or_else(|| format!("env {}: no secret store named {store_name}", entry.name))?;
        let value = store.resolve(key).await?;

        if entry.as_file {
            let path = secrets_dir.join(&entry.name);
            crate::secrets::write_private(&path, &value).map_err(|error| {
                format!(
                    "env {}: writing the secret file failed: {error}",
                    entry.name
                )
            })?;
            resolved.push((format!("{}_FILE", entry.name), path.display().to_string()));
        } else {
            resolved.push((entry.name.clone(), value));
        }
    }

    Ok(resolved)
}

/// Write the edge's record for the next daemon.
/// The service user (ADR 0026) for a daemon that is root: the one named,
/// else `muru`, which must exist and must not be root. A daemon that is
/// not root switches to no one, and says so when a user was named.
fn service_identity(named: Option<&str>) -> anyhow::Result<Option<crate::host::Identity>> {
    if !crate::host::Identity::can_switch() {
        if let Some(name) = named {
            tracing::info!("service user {name} not applied: the daemon is not root");
        }

        return Ok(None);
    }

    let name = named.unwrap_or(SERVICE_USER);
    let identity = crate::host::Identity::lookup(name, None, None).map_err(|error| {
        anyhow::anyhow!(
            "{error:#}: a daemon that runs as root starts the edge and the applications as a \
             service user, never as root. Create the account (useradd --system \
             --no-create-home {name}, or a sysusers.d entry), name another with --user or the \
             [daemon] table's user, or run the daemon as that user instead"
        )
    })?;
    anyhow::ensure!(
        !identity.is_root(),
        "the service user {name} is root: name an unprivileged account with --user or the \
         [daemon] table's user"
    );
    tracing::info!(
        user = name,
        "the edge and applications run as the service user"
    );
    Ok(Some(identity))
}

/// Section 4.11: a route's upload settings for the edge, with their
/// defaults in place. Apply has checked the keys, so a route that fails
/// them here takes no uploads.
fn upload_message(route: &muru_config::Route) -> Option<pb::Upload> {
    let settings = muru_config::check_upload(route).ok().flatten()?;

    Some(pb::Upload {
        mode: settings.mode,
        max_file_size: settings.max_file_size.unwrap_or(0),
        max_files: settings.max_files,
        hash: settings.hash,
        precheck: settings.precheck,
        spool_quota: settings.spool_quota,
    })
}

/// The upload spool, open to the edge's user alone: the service user
/// under a root daemon. The edge makes each application's directory in
/// it.
fn prepare_spool(directory: &Path, service: Option<crate::host::Identity>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(directory)?;

    if let Some(service) = service {
        std::os::unix::fs::chown(directory, Some(service.uid), Some(service.gid))?;
    }

    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
}

/// Give a release's socket directory to the application's user with the
/// service user's group, set-group-id so that the sockets bound in it
/// carry that group, and open to the owner and the group alone.
fn share_sockets(
    directory: &Path,
    owner: crate::host::Identity,
    service: crate::host::Identity,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::os::unix::fs::chown(directory, Some(owner.uid), Some(service.gid))?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o2750))
}

fn write_edge_record(run_dir: &Path, edge: &EdgeProcess) {
    write_record(
        &run_dir.join("edge.toml"),
        &EdgeRecord {
            pid: edge.pid().unwrap_or(0),
            control: edge.control().to_path_buf(),
            listener_socket: edge.listener_socket().to_path_buf(),
            generation: edge.generation(),
            listeners: edge
                .listeners()
                .iter()
                .map(|(address, tls)| ListenerRecord {
                    address: *address,
                    tls: *tls,
                })
                .collect(),
        },
    );
}

/// Section 13.4: the binary an upgrade re-executes from. The daemon was
/// started as `muructl` by name or by path, so the installed binary is
/// the one that name finds on the PATH or that path names now; what the
/// kernel reports as the running executable may be a replaced file.
pub fn installed_executable() -> anyhow::Result<PathBuf> {
    let name = std::env::args_os()
        .next()
        .ok_or_else(|| anyhow::anyhow!("the daemon has no command name"))?;
    let name = PathBuf::from(name);

    let found = if name.components().count() > 1 {
        name.is_file().then_some(name)
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(&name))
                .find(|candidate| candidate.is_file())
        })
    };

    match found {
        Some(path) => Ok(path),
        None => std::env::current_exe().context("finding the installed executable"),
    }
}

/// Check the installed binary runs, and learn its version, before the
/// daemon commits to re-executing from it.
async fn prepare_upgrade() -> Result<Upgrade, String> {
    let executable = installed_executable().map_err(|error| format!("{error:#}"))?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(&executable)
            .arg("--version")
            .output(),
    )
    .await
    .map_err(|_| format!("{} did not report its version", executable.display()))?
    .map_err(|error| format!("{} cannot be run: {error}", executable.display()))?;

    if !output.status.success() {
        return Err(format!(
            "{} failed to report its version: {}",
            executable.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let version = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_owned();

    if version.is_empty() {
        return Err(format!("{} reported no version", executable.display()));
    }

    Ok(Upgrade {
        executable,
        version,
    })
}

/// Replace this process's image with the installed binary, keeping the
/// pid, the children and the environment. Returns only on failure.
fn exec_daemon(executable: &Path, arguments: &[std::ffi::OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;

    std::process::Command::new(executable)
        .args(arguments)
        .exec()
}

/// The [proxy] table as the snapshot carries it, with the defaults of
/// section 4.7 filled in: the four common header types, recursive
/// resolution, and the configured headers kept.
pub(crate) fn proxy_message(proxy: &muru_config::Proxy) -> pb::Proxy {
    let headers = if proxy.headers.is_empty() {
        muru_config::DEFAULT_PROXY_HEADERS
            .iter()
            .map(|header| (*header).to_owned())
            .collect()
    } else {
        proxy.headers.clone()
    };

    pb::Proxy {
        trusted: proxy.trusted.clone(),
        headers,
        recursive: proxy.recursive.unwrap_or(true),
        consume: proxy.forwarded_headers == Some(muru_config::ForwardedHeaders::Consume),
    }
}

/// The node-wide error pages and override as the snapshot carries them
/// (section 4.12), checked; a page file must be an absolute path.
pub(crate) fn error_tables_message(
    pages: &std::collections::BTreeMap<String, muru_config::ErrorPage>,
    override_: Option<&muru_config::ErrorOverride>,
) -> Result<(Vec<pb::ErrorPage>, Option<pb::ErrorOverride>), String> {
    muru_config::check_error_pages(pages, false).map_err(|error| format!("[{error}]"))?;

    if let Some(table) = override_ {
        muru_config::check_error_override(table).map_err(|error| format!("[{error}]"))?;
    }

    Ok((
        error_pages_message(pages, None),
        override_.map(error_override_message),
    ))
}

/// Error pages as the snapshot carries them, a relative file resolved
/// against `base`.
pub fn error_pages_message(
    pages: &std::collections::BTreeMap<String, muru_config::ErrorPage>,
    base: Option<&Path>,
) -> Vec<pb::ErrorPage> {
    pages
        .iter()
        .map(|(key, page)| pb::ErrorPage {
            key: key.clone(),
            file: page
                .file
                .as_ref()
                .map(|file| match base {
                    Some(base) => base.join(file).display().to_string(),
                    None => file.clone(),
                })
                .unwrap_or_default(),
            template: page.template.clone().unwrap_or_default(),
            path: page.path.clone().unwrap_or_default(),
            content_type: page.content_type.clone().unwrap_or_default(),
        })
        .collect()
}

/// An error override as the snapshot carries it.
pub fn error_override_message(table: &muru_config::ErrorOverride) -> pb::ErrorOverride {
    pb::ErrorOverride {
        statuses: table
            .statuses
            .iter()
            .map(|status| u32::from(*status))
            .collect(),
        when: table.when.clone().unwrap_or_default(),
    }
}

/// A route's authentication sub-request as the snapshot carries it
/// (section 4.16). The table was checked when it was applied.
pub fn auth_message(table: &muru_config::Auth) -> pb::Auth {
    let millis = |text: Option<&str>| {
        text.and_then(|text| muru_config::parse_duration(text).ok())
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            })
    };

    pb::Auth {
        app: table.app.clone(),
        upstream: table.upstream.clone(),
        path: table.path.clone(),
        timeout_ms: millis(table.timeout.as_deref()),
        copy_headers: table.copy_headers.clone(),
        cache_ttl_ms: millis(table.cache.as_ref().map(|cache| cache.ttl.as_str())),
        cache_key: table
            .cache
            .as_ref()
            .map(|cache| cache.key.clone())
            .unwrap_or_default(),
    }
}

/// Maintenance mode as the snapshot carries it: whether it is on, and the
/// release's exemptions.
fn maintenance_message(on: bool, table: Option<&muru_config::Maintenance>) -> pb::Maintenance {
    let table = table.cloned().unwrap_or_default();
    pb::Maintenance {
        on,
        exempt_paths: table.exempt_paths,
        exempt_addresses: table.exempt_addresses,
        bypass_cookie: table.bypass_cookie.unwrap_or_default(),
        retry_after_s: table
            .retry_after
            .as_deref()
            .and_then(|text| muru_config::parse_duration(text).ok())
            .map_or(0, |duration| duration.as_secs()),
    }
}

/// The [compression] table as the snapshot carries it, checked: the
/// defaults of section 4.17 where the table is silent.
pub(crate) fn compression_message(
    table: Option<&muru_config::Compression>,
) -> Result<pb::Compression, String> {
    let Some(table) = table else {
        return Ok(muru_edge::compression::Compression::default().to_message());
    };

    let min_size = match &table.min_size {
        Some(text) => {
            muru_config::parse_size(text).map_err(|error| format!("min_size: {error}"))?
        }
        None => 0,
    };

    let message = pb::Compression {
        enabled: table.enabled.unwrap_or(true),
        encodings: table.encodings.clone(),
        min_size,
        types: table.types.clone(),
        level: table.level.clone().unwrap_or_default(),
    };

    // Checked the way the edge will read it, so a bad word is refused
    // here rather than in the edge's log.

    muru_edge::compression::Compression::from_snapshot(&message).map(|policy| policy.to_message())
}

/// A host's certificate chain and key as PEM text, for the snapshot.
fn read_pem_pair(certificate: &str, key: &str) -> anyhow::Result<(String, String)> {
    let chain =
        std::fs::read_to_string(certificate).with_context(|| format!("reading {certificate}"))?;
    let key = std::fs::read_to_string(key).with_context(|| format!("reading {key}"))?;

    Ok((chain, key))
}

/// Read a record a previous daemon wrote, if there is one it can decode.
fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Write a record for the next daemon.
fn write_record<T: serde::Serialize>(path: &Path, record: &T) {
    let written = toml::to_string(record)
        .map_err(std::io::Error::other)
        .and_then(|text| std::fs::write(path, text));

    if let Err(error) = written {
        tracing::error!(%error, path = %path.display(), "writing a record failed");
    }
}

/// Whether a process's command line names the path, which is how a host
/// record is checked against the process that has its pid now.
fn command_mentions(pid: u32, path: &Path) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output();

    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&path.display().to_string()),
        Err(_) => false,
    }
}

/// The index of the worker a link socket belongs to, from its name:
/// `worker-N.sock`, or `worker-GROUP-N.sock` in a worker group.
fn worker_index(socket: &str) -> Option<u32> {
    let name = std::path::Path::new(socket).file_stem()?.to_str()?;
    name.strip_prefix("worker-")?
        .rsplit('-')
        .next()?
        .parse()
        .ok()
}

impl Daemon {
    /// Everything the dashboard shows (section 10.7): the node from the
    /// daemon's own facts, the releases from the store and the hosts,
    /// the requests and pools from the edge, and each serving release's
    /// log tail from its file.
    async fn dashboard_data(&mut self) -> muru_dashboard::NodeData {
        use muru_dashboard::{
            AppRow, EventRow, HostRow, LogPanel, NodeData, NodeRequestRow, ReleaseRow, RolloutRow,
            RouteRow, ScalingRow, Summary,
        };
        use muru_edge::access::{format_bytes, format_duration};

        let recent = match self.edge.recent(200).await {
            Ok(recent) => recent,
            Err(error) => {
                tracing::debug!(%error, "the edge's recent requests could not be read");
                pb::RecentRequests::default()
            }
        };

        let websockets = match self.edge.websockets("", DASHBOARD_SOCKETS).await {
            Ok(report) => report,
            Err(error) => {
                tracing::debug!(%error, "the edge's WebSockets could not be read");
                pb::WebSockets::default()
            }
        };

        // A WebSocket's closing record carries how long it was open, which
        // is not a request's time.

        let mut totals: Vec<u64> = recent
            .records
            .iter()
            .filter(|record| record.websocket_close.is_empty())
            .map(|record| record.total_us)
            .collect();
        totals.sort_unstable();
        let percentile = |fraction: f64| -> String {
            if totals.is_empty() {
                return "-".to_owned();
            }

            let index = ((totals.len() - 1) as f64 * fraction).round() as usize;
            format_duration(Duration::from_micros(totals[index]))
        };
        let summary = Summary {
            rate: format!(
                "{:.1}",
                recent.minute_requests as f64 / recent.minute_seconds.max(1) as f64
            ),
            p50: percentile(0.5),
            p95: percentile(0.95),
            errors: recent.minute_errors,
            in_flight: recent
                .pools
                .iter()
                .map(|pool| pool.busy.saturating_sub(pool.websockets) as usize)
                .sum(),
            websockets: websockets.open as usize,
        };

        let mut apps = Vec::new();
        let maintenance: HashMap<String, u64> = self
            .store
            .maintenance()
            .unwrap_or_default()
            .into_iter()
            .collect();

        for app in self.store.apps().unwrap_or_default() {
            let mut releases = Vec::new();

            for release in self.store.releases(Some(&app.name)).unwrap_or_default() {
                let workers = self
                    .running
                    .get(&(app.name.clone(), release.id.clone()))
                    .map_or(0, Running::workers);
                let pools: Vec<&pb::PoolState> = recent
                    .pools
                    .iter()
                    .filter(|pool| pool.app == app.name && pool.release == release.id)
                    .collect();
                releases.push(ReleaseRow {
                    id: release.id.clone(),
                    state: release.state.as_str().to_owned(),
                    serving: app.current.as_deref() == Some(release.id.as_str()),
                    message: release.message.clone(),
                    workers,
                    slots_busy: pools.iter().map(|pool| pool.busy as usize).sum(),
                    websockets: pools.iter().map(|pool| pool.websockets as usize).sum(),
                    slots_total: pools.iter().map(|pool| pool.slots as usize).sum(),
                    waiting: pools.iter().map(|pool| pool.waiting as usize).sum(),
                    created: clock_hms(release.created_unix_ms),
                });
            }

            releases.reverse();
            apps.push(AppRow {
                name: app.name.clone(),
                current: app.current.clone().unwrap_or_default(),
                previous: app.previous.clone().unwrap_or_default(),
                maintenance_since: maintenance
                    .get(&app.name)
                    .map(|since| clock_hms(*since))
                    .unwrap_or_default(),
                releases,
            });
        }

        // Sections 7.3 and 7.4: each running release's groups as their
        // autoscalers see them.

        let mut scaling = Vec::new();
        let mut running: Vec<&(String, String)> = self.running.keys().collect();
        running.sort();

        for (app, id) in running {
            let measured = |value: Option<f64>, unit: &str| {
                value.map_or("-".to_owned(), |value| format!("{value:.0}{unit}"))
            };

            let optional: Vec<String> = self
                .running
                .get(&(app.clone(), id.clone()))
                .map(|running| {
                    running
                        .groups
                        .iter()
                        .filter(|group| group.optional)
                        .map(|group| group.name.clone())
                        .collect()
                })
                .unwrap_or_default();

            for (group, status) in self.scale_status(app, id).unwrap_or_default() {
                scaling.push(ScalingRow {
                    release: format!("{app}@{id}"),
                    app: app.clone(),
                    id: id.clone(),
                    pinned: status.pinned.is_some(),
                    optional: optional.contains(&group),
                    group,
                    workers: if status.desired == status.ready {
                        status.ready.to_string()
                    } else {
                        format!("{} -> {}", status.ready, status.desired)
                    },
                    policy: status.spec.describe(),
                    cpu: measured(status.workers.worker_cpu.map(|cpu| cpu * 100.0), "%"),
                    gc: measured(status.workers.gc_share.map(|share| share * 100.0), "%"),
                    gil_wait: measured(status.workers.gil_wait_p95_ms, " ms"),
                    loop_lag: measured(status.workers.loop_lag_p95_ms, " ms"),
                    diagnosis: status.diagnosis.summary.to_owned(),
                    advice: status.diagnosis.advice.to_owned(),
                    last_decision: status
                        .last
                        .map(|(age, what)| {
                            format!(
                                "{what} ({} ago)",
                                muru_config::format_duration(Duration::from_secs(age.as_secs()))
                            )
                        })
                        .unwrap_or_default(),
                });
            }
        }

        // Section 9.3: the rollouts under way, then the latest that ended.

        let mut rollouts = self.store.rollouts(None).unwrap_or_default();
        rollouts.sort_by_key(|rollout| {
            (
                !rollout.state.is_active(),
                std::cmp::Reverse(rollout.updated_unix_ms),
            )
        });
        let rollouts = rollouts
            .into_iter()
            .take(10)
            .map(|rollout| RolloutRow {
                percent: rollout.percent(),
                app: rollout.app,
                id: rollout.id,
                candidate: rollout.candidate,
                baseline: rollout.baseline,
                state: rollout.state.as_str().to_owned(),
                message: rollout.message,
                updated: clock_hms(rollout.updated_unix_ms),
            })
            .collect();

        let hosts = self
            .store
            .hosts()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, host)| HostRow {
                name,
                names: host.names,
                listeners: host.listeners,
                tls: host.tls.is_some(),
                default: host.default,
            })
            .collect();
        let routes = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .map(|route| RouteRow {
                host: route.host,
                path: route.path,
                app: route.app,
            })
            .collect();
        let requests = recent
            .records
            .iter()
            .rev()
            .map(|record| NodeRequestRow {
                time: clock_hms(record.started_unix_ms),
                status: record.status as u16,
                status_class: (record.status / 100) as u16,
                reason: if record.websocket_close.is_empty() {
                    record.reason.clone()
                } else {
                    format!("websocket {}", record.websocket_close)
                },
                method: record.method.clone(),
                target: record.target.clone(),
                release: if record.app.is_empty() {
                    "-".to_owned()
                } else {
                    format!("{}@{}", record.app, record.release)
                },
                host: record.host.clone(),
                bytes: format_bytes(record.bytes),
                total: format_duration(Duration::from_micros(record.total_us)),
                queue: format_duration(Duration::from_micros(record.queue_us)),
                application: format_duration(Duration::from_micros(record.application_us)),
                worker: if record.worker < 0 {
                    "-".to_owned()
                } else {
                    record.worker.to_string()
                },
                request_id: record.request_id.clone(),
            })
            .collect();
        let latest = self.store.last_event_sequence().unwrap_or(0);
        let mut events: Vec<EventRow> = self
            .store
            .events_after(latest.saturating_sub(50), 50)
            .unwrap_or_default()
            .into_iter()
            .map(|event| EventRow {
                sequence: event.sequence,
                time: clock_hms(event.time_unix_ms),
                level: event.level.to_owned(),
                kind: event.kind,
                subject: event.subject,
                message: event.message,
            })
            .collect();
        events.reverse();
        let mut logs = Vec::new();

        for app in &apps {
            if app.current.is_empty() {
                continue;
            }

            let path = self.locations.release_log(&app.name, &app.current);
            logs.push(LogPanel {
                app: app.name.clone(),
                release: app.current.clone(),
                lines: crate::logs::last_lines(&path, 40).await,
            });
        }

        NodeData {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            uptime: muru_dashboard::uptime(Duration::from_millis(
                crate::model::now_unix_ms().saturating_sub(self.info.started_unix_ms),
            )),
            listeners: self
                .info
                .listeners
                .read()
                .expect("listeners poisoned")
                .iter()
                .map(|(address, tls)| {
                    format!("{}://{address}", if *tls { "https" } else { "http" })
                })
                .collect(),
            edge_pid: self
                .info
                .edge_pid
                .load(std::sync::atomic::Ordering::Relaxed),
            snapshot_version: self.edge.snapshot_version(),
            metrics: self
                .info
                .metrics
                .map(|address| format!("http://{address}/metrics")),
            summary,
            apps,
            hosts,
            routes,
            scaling,
            rollouts,
            requests,
            websockets: crate::dashboard::websockets_view(websockets),
            events,
            logs,
        }
    }

    /// Section 13.3: read the configuration file again and apply what
    /// changed, each table in its own way: the edge's tables go to it in
    /// a snapshot, changed listeners replace the edge, sinks are started
    /// and stopped, and the rest are swapped in place. The file is
    /// checked whole and the new listeners bound before anything is
    /// applied, so a mistake applies nothing.
    async fn reload(&mut self) -> Result<String, String> {
        let path = self.locations.config.path.clone();
        let settings = crate::settings::NodeSettings::read(
            &path,
            &self.locations.state.path.join("secrets"),
            &self.overrides,
        )?;
        let clients = settings.client_rules.policies(&self.store)?;
        let listeners = self.bind_listeners(&settings.listeners)?;
        let mut report = crate::reload::Report::new(&path);

        // Section 4.4: the tables the edge takes from each snapshot, and
        // the limits with them (section 4.6).

        let mut snapshot = false;

        for (table, changed) in [
            ("[proxy]", settings.proxy != self.proxy),
            ("[compression]", settings.compression != self.compression),
            ("[error_pages]", settings.error_pages != self.error_pages),
            (
                "[error_override]",
                settings.error_override != self.error_override,
            ),
            (
                "[[client_class]]",
                clients.classes_message != self.clients.classes_message,
            ),
            (
                "[rate_limit]",
                clients.limits_message != self.clients.limits_message,
            ),
            ("[limits]", settings.limits != self.limits),
            ("[[listener]] h2c", settings.h2c != self.h2c),
        ] {
            if changed {
                report.changed(table, "the edge has it");
                snapshot = true;
            } else {
                report.unchanged(table);
            }
        }

        self.proxy = settings.proxy;
        self.compression = settings.compression;
        self.error_pages = settings.error_pages;
        self.error_override = settings.error_override;
        self.limits = settings.limits;
        self.h2c = settings.h2c;
        self.client_rules = settings.client_rules;
        self.clients = clients;

        // Section 7.4: the budget, and the dashboard's actions switch.

        if settings.budget != self.budget {
            self.budget = settings.budget;
            report.changed(
                "[daemon.budget]",
                &self.budget.describe(&self.budget_usage()),
            );
        } else {
            report.unchanged("[daemon.budget]");
        }

        let mut defined = self.client_rules.defined();
        defined.budget = self.budget_view(self.budget_usage());
        *self.info.defined.write().expect("defined names poisoned") = defined;
        let actions = settings.dashboard_actions;
        let actions_were = self
            .info
            .dashboard_actions
            .swap(actions, std::sync::atomic::Ordering::Relaxed);

        if actions_were != actions {
            report.changed("[dashboard] actions", if actions { "on" } else { "off" });
        } else {
            report.unchanged("[dashboard] actions");
        }

        // Section 11.5: sinks start, stop and restart on their cursors.

        let sink_changes = self.sinks.reconcile(settings.sinks);

        if sink_changes.is_empty() {
            report.unchanged("[[events.sink]]");
        } else {
            report.changed("[[events.sink]]", &sink_changes.describe());
        }

        // Section 10.8: the stores matter when a release starts, so they
        // are swapped whole; section 11.3: the rotator takes the new
        // policy and keeps what it has seen of each file.

        if settings.secrets != self.secrets {
            self.secrets = settings.secrets;
            let names: Vec<&str> = self
                .secrets
                .iter()
                .map(|store| store.name.as_str())
                .collect();
            report.changed(
                "[secrets.store.NAME]",
                &if names.is_empty() {
                    "no stores".to_owned()
                } else {
                    format!("the stores are {}", names.join(", "))
                },
            );
        } else {
            report.unchanged("[secrets.store.NAME]");
        }

        let rotation = self
            .rotation
            .as_ref()
            .map(|rotator| rotator.policy().clone());

        if settings.rotation != rotation {
            match (settings.rotation, self.rotation.as_mut()) {
                (Some(policy), Some(rotator)) => rotator.set_policy(policy),
                (Some(policy), None) => self.rotation = Some(crate::rotation::Rotator::new(policy)),
                (None, _) => self.rotation = None,
            }

            report.changed(
                "[logs]",
                if self.rotation.is_some() {
                    "the daemon rotates on the new policy"
                } else {
                    "rotation is external"
                },
            );
        } else {
            report.unchanged("[logs]");
        }

        // Section 13.3: what the next stop signal means.

        if settings.shutdown != self.shutdown {
            self.shutdown = settings.shutdown;
            report.changed("[shutdown]", &self.shutdown.describe());
        } else {
            report.unchanged("[shutdown]");
        }

        // Section 4.2: the hosts' certificate files, read again.

        let certificates = self.refresh_certificates(true);

        if certificates.renewed.is_empty() {
            report.unchanged("the hosts' certificates");
        } else {
            report.changed(
                "the hosts' certificates",
                &format!(
                    "{} renewed; the edge has them",
                    certificates.renewed.join(", ")
                ),
            );
            snapshot = true;
        }

        for (host, why) in certificates.kept {
            report.kept(&format!("host {host} keeps its certificate"), &why);
        }

        if snapshot && let Err(error) = self.push_snapshot().await {
            report.failed("the edge's tables", &error);
        }

        // Section 13.4: changed listeners take a new edge, which gets the
        // snapshot just pushed before the old one is told to stop.

        match listeners {
            Some((listeners, what)) => match self.replace_edge(Some(listeners), &what).await {
                Ok(_) => report.changed("[[listener]]", &format!("{what}; the edge was replaced")),
                Err(error) => report.failed("[[listener]]", &error),
            },
            None => report.unchanged("[[listener]]"),
        }

        self.events.emit(
            "config.reloaded",
            "daemon",
            if report.is_ok() { "info" } else { "warning" },
            &report.message(),
        );
        report.outcome()
    }

    /// Section 4.2: read the certificate and key files of the applied
    /// TLS hosts again, every host or only those not read yet, and keep
    /// each pair that is whole and goes together, for the snapshots. A
    /// host whose files cannot be read, or do not go together, keeps its
    /// last good pair, named in a `certificate.unreadable` event; a host
    /// that never had one serves without TLS until the files are right.
    fn refresh_certificates(&mut self, every: bool) -> CertificateChanges {
        let mut changes = CertificateChanges::default();
        let hosts = self.store.hosts().unwrap_or_default();
        self.certificates.retain(|name, _| {
            hosts
                .iter()
                .any(|(host, spec)| host == name && spec.tls.is_some())
        });

        for (name, host) in hosts {
            let Some(tls) = &host.tls else {
                continue;
            };

            if !every && self.certificates.contains_key(&name) {
                continue;
            }

            let read = read_pem_pair(&tls.certificate, &tls.key).and_then(|(chain, key)| {
                let (parsed_chain, parsed_key) = muru_edge::tls::parse_pem(&chain, &key)?;
                muru_edge::tls::certified_key(parsed_chain, parsed_key)?;
                Ok((chain, key))
            });

            match read {
                Ok(pair) => {
                    if self.certificates.get(&name) != Some(&pair)
                        && self.certificates.insert(name.clone(), pair).is_some()
                    {
                        changes.renewed.push(name);
                    }
                }
                Err(error) => {
                    let kept = self.certificates.contains_key(&name);
                    self.events.emit(
                        "certificate.unreadable",
                        &format!("host {name}"),
                        "error",
                        &format!(
                            "{error:#}; {}",
                            if kept {
                                "the host keeps the certificate it had"
                            } else {
                                "the host serves without TLS until the files are right"
                            }
                        ),
                    );
                    changes.kept.push((name, format!("{error:#}")));
                }
            }
        }

        changes
    }

    /// Section 13.3: the sockets for a changed set of listeners, bound
    /// before anything else is applied so that a port in use applies
    /// nothing. A kept listener's socket is the edge's own, so its
    /// backlog survives the replacement. None when the set is unchanged.
    fn bind_listeners(
        &self,
        wanted: &[(SocketAddr, bool)],
    ) -> Result<Option<(Vec<crate::edges::Listener>, String)>, String> {
        let current = self.edge.listeners();

        if wanted.len() == current.len() && wanted.iter().all(|listener| current.contains(listener))
        {
            return Ok(None);
        }

        let mut listeners = Vec::new();

        for &(address, tls) in wanted {
            let socket = match self.edge.socket_on(address) {
                Some(socket) => socket,
                None => std::net::TcpListener::bind(address)
                    .map_err(|error| format!("cannot listen on {address}: {error}"))?,
            };

            listeners.push(crate::edges::Listener { socket, tls });
        }

        let describe = |(address, tls): &(SocketAddr, bool)| {
            format!("{}://{address}", if *tls { "https" } else { "http" })
        };
        let added: Vec<String> = wanted
            .iter()
            .filter(|listener| !current.contains(listener))
            .map(describe)
            .collect();
        let removed: Vec<String> = current
            .iter()
            .filter(|listener| !wanted.contains(listener))
            .map(describe)
            .collect();
        let mut what = Vec::new();

        if !added.is_empty() {
            what.push(format!("{} added", added.join(", ")));
        }

        if !removed.is_empty() {
            what.push(format!("{} removed", removed.join(", ")));
        }

        Ok(Some((listeners, what.join(", "))))
    }

    /// How long an edge is given to finish its connections when it is
    /// replaced: the request deadline, so a response in flight can
    /// complete, and a little over.
    fn drain_grace(&self) -> Duration {
        self.limits.request_deadline + Duration::from_secs(5)
    }

    /// Section 9.10: the routes and the applications in maintenance mode
    /// with the active overlays laid over them, lowest priority first.
    fn effective(&self) -> (Vec<muru_config::Route>, BTreeSet<String>) {
        let overlays = self.store.overlays().unwrap_or_default();
        let states = self.store.overlay_states().unwrap_or_default();
        let active = crate::overlays::ordered(&overlays, &states);
        let mut routes = self.store.routes().unwrap_or_default();
        crate::overlays::apply_routes(&mut routes, &active);
        let base: BTreeSet<String> = self.maintenance.keys().cloned().collect();
        let maintained = crate::overlays::apply_maintenance(&base, &active);
        (routes, maintained)
    }

    /// Section 9.10: whether an overlay's preconditions hold, or why not.
    fn overlay_preconditions(&self, overlay: &muru_config::Overlay) -> Result<(), String> {
        let Some(ready) = overlay
            .preconditions
            .as_ref()
            .and_then(|preconditions| preconditions.release_ready.as_deref())
        else {
            return Ok(());
        };

        let route_apps: std::collections::BTreeMap<String, String> = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|route| route.name.map(|name| (name, route.app)))
            .collect();
        let (app, id) = crate::config::precondition_release(ready, overlay, &route_apps)
            .ok_or_else(|| format!("release_ready {ready:?} names no release"))?;

        // A retained release is warm and takes traffic at once, so it
        // counts as ready.

        match self.store.release(&app, &id) {
            Ok(release)
                if matches!(release.state, ReleaseState::Ready | ReleaseState::Retained) =>
            {
                Ok(())
            }
            Ok(release) => Err(format!("{app}@{id} is {}, not ready", release.state)),
            Err(_) => Err(format!("{app}@{id} does not exist")),
        }
    }

    /// Section 9.10: begin and end overlays by their schedules and by the
    /// time they were activated for, start what their warm-ups name, and
    /// give the edge the result.
    async fn reconcile_overlays(&mut self) {
        let overlays = self.store.overlays().unwrap_or_default();
        let states: HashMap<String, crate::model::OverlayState> = self
            .store
            .overlay_states()
            .unwrap_or_default()
            .into_iter()
            .map(|state| (state.name.clone(), state))
            .collect();
        let now = now_unix_ms();
        let mut changed = false;

        for (name, overlay) in &overlays {
            let subject = format!("overlay {name}");

            match crate::overlays::step(overlay, states.get(name), now) {
                crate::overlays::Step::Stay => {}
                crate::overlays::Step::Release => {
                    let _ = self.store.set_overlay_state(name, None);
                }
                crate::overlays::Step::Extend { until } => {
                    if let Some(state) = states.get(name) {
                        let state = crate::model::OverlayState {
                            until_unix_ms: until,
                            ..state.clone()
                        };
                        let _ = self.store.set_overlay_state(name, Some(&state));
                    }
                }
                crate::overlays::Step::End { reason } => {
                    let _ = self.store.set_overlay_state(name, None);

                    if crate::overlays::is_temporary(name) {
                        let _ = self.store.delete_overlay(name);
                    }

                    let kind = if crate::overlays::is_schedule(name) {
                        "schedule.expired"
                    } else {
                        "overlay.ended"
                    };
                    self.events
                        .emit(kind, &subject, "notice", &format!("ended: {reason}"));
                    changed = true;
                }
                crate::overlays::Step::Begin { until } => {
                    let preconditions = self.overlay_preconditions(overlay);

                    // An advisory overlay only says what it would do, once
                    // a window; a skipped one is not retried every second.

                    let (source, kind, level, detail) =
                        match (overlay.advisory == Some(true), &preconditions) {
                            (true, Ok(())) => (
                                "advised",
                                "overlay.would_activate",
                                "notice",
                                "would activate: its schedule's window began; it is advisory"
                                    .to_owned(),
                            ),
                            (true, Err(why)) => (
                                "advised",
                                "overlay.would_activate",
                                "notice",
                                format!("would be skipped: {why}; it is advisory"),
                            ),
                            (false, Ok(())) if crate::overlays::is_schedule(name) => (
                                "schedule",
                                "schedule.activated",
                                "notice",
                                "activated: its window began".to_owned(),
                            ),
                            (false, Ok(())) => (
                                "schedule",
                                "overlay.activated",
                                "notice",
                                "activated: its schedule's window began".to_owned(),
                            ),
                            (false, Err(why)) => (
                                "skipped",
                                "overlay.skipped",
                                "warning",
                                format!("not activated: {why}"),
                            ),
                        };

                    let active = source == "schedule";
                    let latched = active && overlay.latch == Some(true);
                    let state = crate::model::OverlayState {
                        name: name.clone(),
                        active,
                        source: source.to_owned(),
                        since_unix_ms: now,
                        until_unix_ms: if latched { None } else { until },
                    };
                    let _ = self.store.set_overlay_state(name, Some(&state));
                    self.events.emit(kind, &subject, level, &detail);
                    changed |= active;
                }
            }
        }

        let warm_ups = self.warm_ups(&overlays, now);

        if warm_ups != self.warm_ups {
            for (name, release) in &warm_ups {
                if self.warm_ups.contains_key(name) {
                    continue;
                }

                let starts = overlays
                    .iter()
                    .find(|(declared, _)| declared == name)
                    .and_then(|(_, overlay)| crate::overlays::window(overlay, now))
                    .map_or(0, |(start, _)| start.saturating_sub(now));
                let mut detail = format!(
                    "warming up: its window begins in {}",
                    crate::overlays::describe_wait(starts)
                );

                if let Some((app, id)) = release {
                    detail.push_str(&format!("; starting {app}@{id}"));
                }

                self.events.emit(
                    "overlay.warming",
                    &format!("overlay {name}"),
                    "info",
                    &detail,
                );
            }

            let releases_changed = warm_ups.values().collect::<BTreeSet<_>>()
                != self.warm_ups.values().collect::<BTreeSet<_>>();
            self.warm_ups = warm_ups;

            if releases_changed && !changed {
                self.reconcile_traffic().await;
            }
        }

        if changed {
            self.reconcile_traffic().await;

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot for an overlay failed");
            }
        }
    }

    /// Section 9.10: warn, every ten minutes while schedules are declared,
    /// when the system clock is not synchronised, as their windows follow
    /// it, and say so once it is again.
    fn check_clock(&mut self) {
        let (checked, warned) = self.clock;

        if checked.is_some_and(|at| at.elapsed() < CLOCK_CHECK) {
            return;
        }

        let scheduled = self
            .store
            .overlays()
            .unwrap_or_default()
            .iter()
            .any(|(_, overlay)| {
                overlay
                    .trigger
                    .as_ref()
                    .is_some_and(|trigger| trigger.schedule.is_some())
            });
        let event = clock_event(clock_synchronised(), warned, scheduled);
        self.clock = (Some(Instant::now()), warned);

        if let Some((kind, level, message)) = event {
            self.clock.1 = kind == "clock.unsynchronised";
            self.events.emit(kind, "daemon", level, message);
        }
    }

    /// Section 9.10: the overlays in the lead before a window of their
    /// schedule, with the release each warm-up starts.
    fn warm_ups(
        &self,
        overlays: &[(String, muru_config::Overlay)],
        now: u64,
    ) -> BTreeMap<String, Option<(String, String)>> {
        let route_apps: BTreeMap<String, String> = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|route| route.name.map(|name| (name, route.app)))
            .collect();

        overlays
            .iter()
            .filter(|(_, overlay)| crate::overlays::warming(overlay, now))
            .map(|(name, overlay)| {
                let release = overlay
                    .warm_up
                    .as_ref()
                    .and_then(|warm_up| warm_up.start_release.as_deref())
                    .and_then(|release| {
                        crate::config::precondition_release(release, overlay, &route_apps)
                    });
                (name.clone(), release)
            })
            .collect()
    }

    /// Section 7.5: record and activate the temporary overlay `muructl
    /// scale --min` stands for, replacing one already there for the group.
    async fn scale_floor(
        &mut self,
        app: &str,
        group: &str,
        min_workers: u32,
        max_workers: Option<u32>,
        until: u64,
    ) -> Result<String, String> {
        self.store.app(app).map_err(|error| error.to_string())?;
        let now = now_unix_ms();

        if until <= now {
            return Err("--until is not in the future".to_owned());
        }

        let change = muru_config::AppOverlay {
            group: (group != muru_edge::routing::DEFAULT_GROUP).then(|| group.to_owned()),
            min_workers: Some(min_workers),
            max_workers,
            ..muru_config::AppOverlay::default()
        };
        let overlay = muru_config::Overlay {
            apps: BTreeMap::from([(app.to_owned(), change.clone())]),
            ..muru_config::Overlay::default()
        };
        muru_config::check_overlay("scale", &overlay)?;
        let name = crate::overlays::temporary_name(app, group);
        self.store
            .put_overlay(&name, &overlay)
            .map_err(|error| error.to_string())?;
        let state = crate::model::OverlayState {
            name: name.clone(),
            active: true,
            source: "manual".to_owned(),
            since_unix_ms: now,
            until_unix_ms: Some(until),
        };
        self.store
            .set_overlay_state(&name, Some(&state))
            .map_err(|error| error.to_string())?;
        let detail = format!(
            "activated by muructl scale: {group} {} for {}",
            crate::overlays::describe_capacity(&change),
            crate::overlays::describe_wait(until - now)
        );
        self.events.emit(
            "overlay.activated",
            &format!("overlay {name}"),
            "notice",
            &detail,
        );
        Ok(detail)
    }

    /// Section 9.10: what a person, or with `external` the signed webhook,
    /// asks of an overlay.
    async fn control_overlay(
        &mut self,
        name: &str,
        action: OverlayAction,
        external: bool,
    ) -> Result<String, String> {
        let overlay = self
            .store
            .overlays()
            .unwrap_or_default()
            .into_iter()
            .find(|(declared, _)| declared == name)
            .map(|(_, overlay)| overlay)
            .ok_or_else(|| format!("no overlay is named {name}"))?;
        let state = self
            .store
            .overlay_states()
            .unwrap_or_default()
            .into_iter()
            .find(|state| state.name == name);
        let active = state.as_ref().is_some_and(|state| state.active);
        let subject = format!("overlay {name}");
        let now = now_unix_ms();

        let (source, by) = if external {
            ("external", "by the webhook")
        } else {
            ("manual", "by hand")
        };

        let message = match action {
            OverlayAction::Activate { until } => {
                if active {
                    return Err(format!("overlay {name} is already active"));
                }

                self.overlay_preconditions(&overlay)
                    .map_err(|why| format!("overlay {name} was not activated: {why}"))?;
                let state = crate::model::OverlayState {
                    name: name.to_owned(),
                    active: true,
                    source: source.to_owned(),
                    since_unix_ms: now,
                    until_unix_ms: until,
                };
                self.store
                    .set_overlay_state(name, Some(&state))
                    .map_err(|error| error.to_string())?;
                let detail = format!("activated {by}");
                self.events
                    .emit("overlay.activated", &subject, "notice", &detail);
                detail
            }
            OverlayAction::End => {
                if !active {
                    return Err(format!("overlay {name} is not active"));
                }

                // Ended inside its schedule's window, it stays ended until
                // the window is over; otherwise it is simply inactive.

                let window = crate::overlays::window(&overlay, now)
                    .filter(|(start, end)| now >= *start && end.is_none_or(|end| now < end));
                let held = window.map(|(_, end)| crate::model::OverlayState {
                    name: name.to_owned(),
                    active: false,
                    source: "ended".to_owned(),
                    since_unix_ms: now,
                    until_unix_ms: end,
                });
                self.store
                    .set_overlay_state(name, held.as_ref())
                    .map_err(|error| error.to_string())?;

                if crate::overlays::is_temporary(name) {
                    self.store
                        .delete_overlay(name)
                        .map_err(|error| error.to_string())?;
                }

                let detail = format!("ended {by}");
                self.events
                    .emit("overlay.ended", &subject, "notice", &detail);
                detail
            }
            OverlayAction::Promote => {
                if !active {
                    return Err(format!("overlay {name} is not active; activate it first"));
                }

                // Its changes become the base: the routes as the overlay
                // leaves them, with those it adds for client classes, and
                // its maintenance settings.

                let pair = (name.to_owned(), overlay.clone());
                let before = self.store.routes().unwrap_or_default();
                let mut changed = before.clone();
                crate::overlays::apply_routes(&mut changed, &[&pair]);

                for route in changed.iter().filter(|route| !before.contains(route)) {
                    self.store
                        .put_route(route)
                        .map_err(|error| error.to_string())?;
                }

                for (app, change) in &overlay.apps {
                    if let Some(on) = change.maintenance {
                        let since = on.then_some(now);
                        self.store
                            .set_maintenance(app, since)
                            .map_err(|error| error.to_string())?;

                        match since {
                            Some(since) => {
                                self.maintenance.entry(app.clone()).or_insert(since);
                            }
                            None => {
                                self.maintenance.remove(app);
                            }
                        }
                    }
                }

                let held = crate::model::OverlayState {
                    name: name.to_owned(),
                    active: false,
                    source: "promoted".to_owned(),
                    since_unix_ms: now,
                    until_unix_ms: None,
                };
                self.store
                    .set_overlay_state(name, Some(&held))
                    .map_err(|error| error.to_string())?;
                let detail = "promoted: its changes are now the base configuration".to_owned();
                self.events
                    .emit("overlay.promoted", &subject, "notice", &detail);
                detail
            }
        };

        self.reconcile_traffic().await;
        self.push_snapshot().await?;
        Ok(message)
    }

    /// Section 4.14: read the client classes' list files again when one
    /// has changed, and give the edge the result.
    async fn refresh_client_lists(&mut self) {
        match self.clients.refresh() {
            Ok(changed) if changed.is_empty() => {}
            Ok(changed) => {
                let names: Vec<String> = changed
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                self.events.emit(
                    "client_class.list_reloaded",
                    "daemon",
                    "info",
                    &format!("read {} again", names.join(", ")),
                );

                if let Err(error) = self.push_snapshot().await {
                    tracing::error!(%error, "pushing the snapshot with the reloaded lists failed");
                }
            }
            Err(error) => {
                // The classes the edge has stay until the list reads.

                tracing::warn!(%error, "a client class list could not be read; the edge keeps the last one read");
            }
        }
    }

    /// Section 4.15: a rate limit that begins refusing or holding
    /// requests, after a quiet minute, is an event.
    fn watch_rate_limits(&mut self, samples: Option<&[muru_edge::metrics::Sample]>) {
        let Some(samples) = samples else {
            return;
        };

        for (limit, count) in self.enforcement.observe(samples, Instant::now()) {
            self.events.emit(
                "ratelimit.enforcing",
                &format!("rate_limit {limit}"),
                "warning",
                &format!("began refusing or holding requests: {count:.0} so far"),
            );
        }
    }

    /// The node's metrics: the edge's samples with the daemon's own
    /// gauges added, as the Prometheus text exposition.
    async fn metrics(&self) -> String {
        let mut samples = match self.edge.metrics().await {
            Ok(samples) => samples,
            Err(error) => {
                tracing::warn!(%error, "the edge's metrics could not be read");
                Vec::new()
            }
        };

        for ((app, id), running) in &self.running {
            let mut groups = match &running.backend {
                Backend::Servers(servers) => vec![(
                    muru_edge::routing::SERVER_GROUP.to_owned(),
                    servers.running(),
                )],
                Backend::Host(_) | Backend::Dormant => vec![(
                    muru_edge::routing::DEFAULT_GROUP.to_owned(),
                    running.sockets.len(),
                )],
            };

            for group in &running.groups {
                groups.push((group.name.clone(), group.sockets.len()));
            }

            for (group, workers) in groups {
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_workers_ready",
                    &[
                        ("app", app.as_str()),
                        ("release", id.as_str()),
                        ("group", group.as_str()),
                    ],
                    workers as f64,
                ));
            }

            // Section 7.4: what the autoscaler wants and has done.

            for scaling in &running.scalers {
                let labels = [
                    ("app", app.as_str()),
                    ("release", id.as_str()),
                    ("group", scaling.name.as_str()),
                ];
                let (outs, ins) = scaling.scaler.status(Instant::now()).decisions;
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_workers_desired",
                    &labels,
                    scaling.scaler.desired() as f64,
                ));
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_churn_reaps_total",
                    &labels,
                    scaling.scaler.reaps() as f64,
                ));

                for (direction, count) in [("out", outs), ("in", ins)] {
                    samples.push(muru_edge::metrics::Sample::new(
                        "muru_scaling_decisions_total",
                        &[
                            ("app", app.as_str()),
                            ("release", id.as_str()),
                            ("group", scaling.name.as_str()),
                            ("direction", direction),
                        ],
                        count as f64,
                    ));
                }
            }

            // Sections 7.6 and 7.7: what each worker last reported.

            for worker in &running.status {
                let index = worker.index.to_string();
                let labels = [
                    ("app", app.as_str()),
                    ("release", id.as_str()),
                    ("group", worker.group.as_str()),
                    ("worker", index.as_str()),
                ];

                if let Some(cpu_ms) = worker.cpu_ms {
                    samples.push(muru_edge::metrics::Sample::new(
                        "muru_worker_cpu_seconds_total",
                        &labels,
                        cpu_ms as f64 / 1000.0,
                    ));
                }

                samples.push(muru_edge::metrics::Sample::new(
                    "muru_worker_requests_total",
                    &labels,
                    worker.requests as f64,
                ));
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_worker_requests_in_flight",
                    &labels,
                    worker.in_flight as f64,
                ));

                // Section 7.3: the worker's own signals, as histograms
                // once it has observed anything.

                samples.extend(muru_edge::metrics::signal_histogram(
                    "muru_gil_wait_seconds",
                    &labels,
                    &worker.gil_wait,
                    worker.gil_wait_us,
                ));
                samples.extend(muru_edge::metrics::signal_histogram(
                    "muru_loop_lag_seconds",
                    &labels,
                    &worker.loop_lag,
                    worker.loop_lag_us,
                ));

                // Section 11.1: its garbage collections, by generation.

                for generation in &worker.gc {
                    let labels = [
                        ("app", app.as_str()),
                        ("release", id.as_str()),
                        ("group", worker.group.as_str()),
                        ("worker", index.as_str()),
                        ("generation", generation.generation.as_str()),
                    ];

                    for (name, value) in [
                        ("muru_gc_collections_total", generation.collections as f64),
                        ("muru_gc_seconds_total", generation.nanos as f64 / 1e9),
                        (
                            "muru_gc_collected_objects_total",
                            generation.collected as f64,
                        ),
                    ] {
                        samples.push(muru_edge::metrics::Sample::new(name, &labels, value));
                    }

                    for (requests, counts, sum_us) in [
                        ("none", &generation.idle, generation.idle_us),
                        ("in_flight", &generation.busy_pauses, generation.busy_us),
                    ] {
                        let mut with_requests = labels.to_vec();
                        with_requests.push(("requests", requests));
                        samples.extend(muru_edge::metrics::signal_histogram(
                            "muru_gc_pause_seconds",
                            &with_requests,
                            counts,
                            sum_us,
                        ));
                    }
                }

                for (measure, bytes) in [
                    ("rss", worker.rss),
                    ("pss", worker.pss),
                    ("uss", worker.uss),
                ] {
                    if let Some(bytes) = bytes {
                        samples.push(muru_edge::metrics::Sample::new(
                            "muru_worker_memory_bytes",
                            &[
                                ("app", app.as_str()),
                                ("release", id.as_str()),
                                ("group", worker.group.as_str()),
                                ("worker", index.as_str()),
                                ("measure", measure),
                            ],
                            bytes as f64,
                        ));
                    }
                }
            }

            // Sections 7.6 and 7.7: workers replaced and workers gone.

            for (name, counts) in [
                ("muru_worker_recycles_total", &running.recycles),
                ("muru_worker_exits_total", &running.exits),
            ] {
                for ((group, reason), count) in counts {
                    samples.push(muru_edge::metrics::Sample::new(
                        name,
                        &[
                            ("app", app.as_str()),
                            ("release", id.as_str()),
                            ("group", group.as_str()),
                            ("reason", reason.as_str()),
                        ],
                        *count as f64,
                    ));
                }
            }
        }

        muru_edge::metrics::render(&muru_edge::metrics::aggregate(samples))
    }

    /// Re-adopt the hosts a previous daemon left running (section 10.2),
    /// and start again the release that served if its host is gone.
    async fn restore(&mut self) {
        self.maintenance = self
            .store
            .maintenance()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let apps = match self.store.apps() {
            Ok(apps) => apps,
            Err(error) => {
                tracing::error!(%error, "reading the applications failed");
                return;
            }
        };

        for app in apps {
            // A release in a running state is taken over when its host is
            // still there and listening; otherwise it is gone with the
            // previous daemon.

            if let Ok(releases) = self.store.releases(Some(&app.name)) {
                for release in releases.iter().filter(|release| release.state.is_running()) {
                    if self.adopt_host(&release.app, &release.id).await {
                        continue;
                    }

                    let _ = self.store.set_release_state(
                        &release.app,
                        &release.id,
                        ReleaseState::Stopped,
                        "the daemon restarted",
                    );
                }
            }

            let Some(current) = app.current.clone() else {
                continue;
            };
            if self
                .running
                .contains_key(&(app.name.clone(), current.clone()))
            {
                self.switch_to(&app.name, &current).await;
                continue;
            }

            match self.start(&app.name, &current).await {
                Ok(()) => self.switch_to(&app.name, &current).await,
                Err(error) => {
                    self.events.emit(
                        "release.failed",
                        &format!("{}@{current}", app.name),
                        "error",
                        &format!("could not restart after the daemon restarted: {error}"),
                    );
                }
            }
        }
    }

    /// Take over a release's host, or a managed server's instances, from
    /// its record, if it is still there.
    async fn adopt_host(&mut self, app: &str, id: &str) -> bool {
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);

        if let Some(record) = read_record::<ServersRecord>(&run_dir.join("servers.toml")) {
            return self.adopt_servers(app, id, &run_dir, &record).await;
        }

        let Some(record) = read_record::<HostRecord>(&run_dir.join("host.toml")) else {
            return false;
        };

        if !command_mentions(record.pid, &run_dir) {
            return false;
        }

        let group_sockets = record
            .groups
            .iter()
            .map(|group| (group.name.clone(), group.sockets.clone()))
            .collect();
        let optional = record
            .groups
            .iter()
            .filter(|group| group.optional)
            .map(|group| group.name.clone())
            .collect();
        let adopted = HostProcess::adopt(
            record.pid,
            record.sockets.clone(),
            group_sockets,
            optional,
            run_dir.join("r1"),
            1,
        );

        match adopted {
            Ok(host) => {
                let sockets = host.default_sockets();
                let retained_until = matches!(
                    self.store.release(app, id).map(|release| release.state),
                    Ok(ReleaseState::Retained)
                )
                .then(|| Instant::now() + RETENTION);
                let now = Instant::now();
                let pins = self.store.scale_pins(app, id).unwrap_or_default();
                let scalers = self
                    .store
                    .release(app, id)
                    .map(|release| scalers_for(&release.spec, &sockets, &record.groups, &pins, now))
                    .unwrap_or_default();
                let ready_at = record
                    .sockets
                    .iter()
                    .map(|socket| (socket.clone(), now))
                    .collect();
                self.running.insert(
                    (app.to_owned(), id.to_owned()),
                    Running {
                        backend: Backend::Host(host),
                        sockets,
                        groups: record.groups.clone(),
                        retained_until,
                        sticky_until: None,
                        held: false,
                        control: None,
                        status: Vec::new(),
                        recycles: BTreeMap::new(),
                        exits: BTreeMap::new(),
                        signals_from: None,
                        scalers,
                        host_pss: None,
                        idle_since: None,
                        ready_at,
                    },
                );
                self.attach_control(app, id);
                self.events.emit(
                    "release.adopted",
                    &format!("{app}@{id}"),
                    "info",
                    &format!("re-adopted the host (pid {})", record.pid),
                );
                true
            }
            Err(error) => {
                tracing::info!(%error, "release {app}@{id} could not be re-adopted");
                false
            }
        }
    }

    /// Section 9.9: take over a managed server's instances from their
    /// record, with what a restart of one would need resolved again.
    async fn adopt_servers(
        &mut self,
        app: &str,
        id: &str,
        run_dir: &Path,
        record: &ServersRecord,
    ) -> bool {
        let Ok(release) = self.store.release(app, id) else {
            return false;
        };
        let Some(http) = release.spec.http.clone() else {
            return false;
        };

        let subject = format!("{app}@{id}");

        let identity = match self.resolve_identity(app, &subject) {
            Ok(identity) => identity,
            Err(error) => {
                tracing::info!(%error, "release {subject} could not be re-adopted");
                return false;
            }
        };

        let env =
            match resolve_env(&self.secrets, &release.spec.env, &run_dir.join("secrets")).await {
                Ok(env) => env,
                Err(error) => {
                    tracing::info!(%error, "release {subject} could not be re-adopted");
                    return false;
                }
            };
        let adopted = Servers::adopt(
            &http,
            &run_dir.join("r1"),
            &release.spec.source,
            env,
            identity,
            &self.host_log(app, id),
            record,
        );

        match adopted {
            Ok(servers) => {
                let retained_until = matches!(release.state, ReleaseState::Retained)
                    .then(|| Instant::now() + RETENTION);
                let pids: Vec<String> = record
                    .instances
                    .iter()
                    .map(|instance| instance.pid.to_string())
                    .collect();
                self.running.insert(
                    (app.to_owned(), id.to_owned()),
                    Running {
                        backend: Backend::Servers(servers),
                        sockets: Vec::new(),
                        groups: Vec::new(),
                        retained_until,
                        sticky_until: None,
                        held: false,
                        control: None,
                        status: Vec::new(),
                        recycles: BTreeMap::new(),
                        exits: BTreeMap::new(),
                        signals_from: None,
                        scalers: Vec::new(),
                        host_pss: None,
                        idle_since: None,
                        ready_at: HashMap::new(),
                    },
                );
                self.events.emit(
                    "release.adopted",
                    &subject,
                    "info",
                    &format!("re-adopted the server instances (pid {})", pids.join(", ")),
                );
                true
            }
            Err(error) => {
                tracing::info!(%error, "release {subject} could not be re-adopted");
                false
            }
        }
    }

    /// Leave every host and the edge running, for the next daemon.
    fn detach(&mut self) {
        let running = std::mem::take(&mut self.running);

        for (_, running) in running {
            running.backend.release();
        }
    }

    async fn carry_out(&mut self, command: Command) {
        match command {
            Command::Deploy {
                app,
                spec,
                id,
                retain,
                hold,
                created,
                done,
            } => {
                let outcome = self.deploy(&app, *spec, id, retain, hold, created).await;
                let _ = done.send(outcome);
            }
            Command::Rollback { app, done } => {
                let _ = done.send(self.rollback(&app).await);
            }
            Command::StartRollout {
                app,
                release,
                steps,
                gates,
                approve_each_step,
                done,
            } => {
                let outcome = self
                    .start_rollout(&app, &release, steps, gates, approve_each_step)
                    .await;
                let _ = done.send(outcome);
            }
            Command::ControlRollout { app, action, done } => {
                let _ = done.send(self.control_rollout(&app, action).await);
            }
            Command::PromoteRelease { app, id, done } => {
                let _ = done.send(self.promote_release(&app, &id).await);
            }
            Command::SetScale {
                app,
                id,
                group,
                processes,
                unpin,
                done,
            } => {
                let _ = done.send(self.set_scale(&app, &id, &group, processes, unpin));
            }
            Command::GetScaleStatus { app, id, done } => {
                let _ = done.send(self.scale_status(&app, &id));
            }
            Command::SetMaintenance { app, on, done } => {
                let _ = done.send(self.set_maintenance(&app, on).await);
            }
            Command::Drain { app, id, done } => {
                let _ = done.send(self.drain_release(&app, &id).await);
            }
            Command::Reconfigure { done } => {
                // An apply may have changed the applied classes and
                // limits.

                match self.client_rules.policies(&self.store) {
                    Ok(clients) => self.clients = clients,
                    Err(error) => {
                        tracing::warn!(%error, "the client classes stay as they were");
                    }
                }

                self.reconcile_traffic().await;
                self.refresh_certificates(true);
                let _ = done.send(self.push_snapshot().await);
            }
            Command::RefreshCertificates { done } => {
                let changes = self.refresh_certificates(true);

                if !changes.renewed.is_empty()
                    && let Err(error) = self.push_snapshot().await
                {
                    tracing::warn!(%error, "pushing the renewed certificates failed");
                }

                let _ = done.send(changes);
            }
            Command::Reload { done } => {
                let _ = done.send(self.reload().await);
            }
            Command::ReopenLogs { workers, done } => {
                let _ = done.send(self.reopen_logs(workers));
            }
            Command::Metrics { done } => {
                let _ = done.send(self.metrics().await);
            }
            Command::WebSockets { app, limit, done } => {
                let _ = done.send(self.edge.websockets(&app, limit).await);
            }
            Command::Dashboard { done } => {
                let _ = done.send(self.dashboard_data().await);
            }
            Command::HostAttached { app, id, control } => {
                if let Some(running) = self.running.get_mut(&(app, id)) {
                    running.control = Some(control);
                }
            }
            Command::HostMessage { app, id, message } => {
                self.host_message(&app, &id, message).await;
            }
            Command::Debug {
                app,
                id,
                op,
                worker,
                args,
                done,
            } => {
                // Section 11.7: exec runs code in a worker, and so does a
                // debugger session, so both are off unless the daemon's
                // file allows them, and the operations that reach into a
                // worker are recorded.

                if matches!(op.as_str(), "exec" | "attach" | "attach_io" | "detach")
                    && !self.debug_exec
                {
                    let what = if op == "exec" {
                        "debug exec"
                    } else {
                        "debugger attach"
                    };
                    let _ = done.send(Err(format!(
                        "{what} is off: set debug_exec = true in the [daemon] table of the \
                         daemon's configuration file"
                    )));
                    return;
                }

                if op == "attach" {
                    let whom = worker.map_or_else(
                        || "every worker".to_owned(),
                        |index| format!("worker {index}"),
                    );
                    self.events.emit(
                        "debug.attached",
                        &format!("{app}@{id}"),
                        "warning",
                        &format!("a debugger session opened in {whom}"),
                    );
                }

                if matches!(op.as_str(), "profile" | "trace" | "memory" | "exec") {
                    let subject = format!("{app}@{id}");
                    let whom = worker.map_or_else(
                        || "every worker".to_owned(),
                        |index| format!("worker {index}"),
                    );

                    let detail = if op == "exec" {
                        let code: String = args
                            .get("code")
                            .and_then(|code| code.as_str())
                            .unwrap_or_default()
                            .chars()
                            .take(200)
                            .collect();
                        format!("ran code in {whom}: {code}")
                    } else {
                        let seconds = args
                            .get("duration_s")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0);
                        format!("{op} of {whom} for {seconds} s")
                    };

                    let level = if op == "exec" { "warning" } else { "info" };
                    self.events
                        .emit(&format!("debug.{op}"), &subject, level, &detail);
                }

                let control = self
                    .running
                    .get(&(app.clone(), id.clone()))
                    .and_then(|running| running.control.as_ref());

                match control {
                    Some(control) => {
                        self.last_request += 1;
                        control.request(self.last_request, &op, worker, &args);
                        self.asked.insert(self.last_request, done);
                    }
                    None => {
                        let _ = done.send(Err(format!(
                            "release {app}@{id} is not running with a host control connection"
                        )));
                    }
                }
            }
            Command::Overlay {
                name,
                action,
                external,
                done,
            } => {
                let _ = done.send(self.control_overlay(&name, action, external).await);
            }
            Command::ScaleFloor {
                app,
                group,
                min_workers,
                max_workers,
                until,
                done,
            } => {
                let outcome = self
                    .scale_floor(&app, &group, min_workers, max_workers, until)
                    .await;
                let _ = done.send(outcome);
            }
            Command::Shutdown { .. } | Command::Upgrade { .. } => {}
        }
    }

    /// Blue/green (section 9.2): record the release, start it, move traffic
    /// once it is ready, drain the release that served before.
    async fn deploy(
        &mut self,
        app: &str,
        spec: ReleaseSpec,
        id: Option<String>,
        retain: Retention,
        hold: bool,
        created: oneshot::Sender<Result<Release, String>>,
    ) -> Result<Release, String> {
        if let Err(error) = self.make_room(app, &spec).await {
            let _ = created.send(Err(error.clone()));
            return Err(error);
        }

        let record = self.create(app, spec, id);

        let release = match record {
            Ok(release) => release,
            Err(error) => {
                let _ = created.send(Err(error.clone()));
                return Err(error);
            }
        };

        let _ = created.send(Ok(release.clone()));
        let subject = format!("{app}@{}", release.id);
        self.events.emit(
            "release.created",
            &subject,
            "info",
            &format!("from {}", release.spec.source.display()),
        );

        self.start(app, &release.id).await?;

        // Section 9.2: held, the release runs and can be previewed, and
        // traffic stays where it is until a rollout or a promotion.

        if hold {
            if let Some(running) = self.running.get_mut(&(app.to_owned(), release.id.clone())) {
                running.held = true;
            }

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot for a held release failed");
            }

            self.mark_ready(app, &release.id, "held: not serving");

            return self
                .store
                .release(app, &release.id)
                .map_err(|error| error.to_string());
        }

        // Traffic moves, then the previous release drains.

        let previous = self
            .store
            .app(app)
            .map_err(|error| error.to_string())?
            .current;
        self.switch_to(app, &release.id).await;
        self.store
            .set_app_releases(app, Some(&release.id), previous.as_deref())
            .map_err(|error| error.to_string())?;

        if let Some(previous) = previous.filter(|previous| previous != &release.id)
            && !self
                .referenced_releases()
                .contains(&(app.to_owned(), previous.clone()))
        {
            self.drain(app, &previous, retain).await;
        }

        self.store
            .release(app, &release.id)
            .map_err(|error| error.to_string())
    }

    /// Record a release from its spec, with a generated id unless one is given.
    fn create(&self, app: &str, spec: ReleaseSpec, id: Option<String>) -> Result<Release, String> {
        if !valid_name(app) {
            return Err(format!(
                "{app:?} is not a valid application name: lowercase letters, digits and hyphens, up to 63 characters"
            ));
        }

        self.store
            .ensure_app(app)
            .map_err(|error| error.to_string())?;

        let id = match id {
            Some(id) if valid_name(&id) => id,
            Some(id) => return Err(format!("{id:?} is not a valid release id")),
            None => self
                .store
                .next_release_id(app)
                .map_err(|error| error.to_string())?,
        };

        if self.store.release(app, &id).is_ok() {
            return Err(format!("release {app}@{id} already exists"));
        }

        self.store
            .create_release(app, &id, &spec)
            .map_err(|error| error.to_string())
    }

    /// Section 8.2: build the release's environment when its spec says
    /// so and it is not there yet (a release started again after a daemon
    /// restart, or a rollback target, keeps the one it has), as the
    /// release's user, with uv's lines in the release log tagged [build].
    /// Returns the interpreter the host runs: the environment's, or the
    /// spec's when nothing is built.
    async fn build_environment(
        &self,
        app: &str,
        id: &str,
        release: &Release,
        identity: Option<crate::host::Identity>,
    ) -> Result<PathBuf, String> {
        let spec = &release.spec;

        if !spec.build.builds {
            return Ok(spec.python.clone());
        }

        let environment =
            crate::provision::Provisioning::environment_dir(&self.locations.state.path, app, id);
        let python = environment.join("bin").join("python");

        if python.is_file() {
            return Ok(python);
        }

        let Some(uv) = self.info.provisioning.uv.clone() else {
            return Err(
                "the release needs an environment built and uv was not found; install uv, or                  deploy a source with its environment built"
                    .to_owned(),
            );
        };
        let subject = format!("{app}@{id}");
        self.set_state(release, ReleaseState::Building, "building the environment");
        self.events.emit(
            "release.building",
            &subject,
            "info",
            &format!("from {}", spec.source.join("uv.lock").display()),
        );

        // The directories the build writes, owned by the release's user:
        // the application's environments and a uv cache of its own.

        let user =
            identity.map_or_else(|| "daemon".to_owned(), |identity| identity.uid.to_string());
        let cache = self.locations.state.path.join("uv-cache").join(&user);
        let parent = environment
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.locations.state.path.clone());

        for dir in [&parent, &cache] {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("creating {}: {error}", dir.display()))?;

            if let Some(identity) = identity {
                std::os::unix::fs::chown(dir, Some(identity.uid), Some(identity.gid))
                    .map_err(|error| format!("owning {}: {error}", dir.display()))?;
            }
        }

        let isolation = self.info.provisioning.build_isolation(cache);
        let build = muru_provision::Build {
            project: spec.source.clone(),
            python: Some(spec.python.display().to_string()),
            deployment: true,
            deps_only: false,
            target: Some(environment.clone()),
            extras: spec.build.extras.clone(),
            groups: spec.build.groups.clone(),
            source_builds: spec.build.source_builds,
            compile_bytecode: spec.build.compile_bytecode,
        };
        let log_path = self.host_log(app, id);
        let started = std::time::Instant::now();
        let digest = muru_provision::build::lock_digest(&spec.source).unwrap_or_default();
        let outcome = tokio::task::spawn_blocking(move || {
            use std::io::Write;

            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map_err(|error| format!("opening {}: {error}", log_path.display()))?;
            let mut command = build.command(&uv, &isolation);
            crate::host::own_session_std(&mut command, identity);
            let _ = writeln!(log, "[build] {}", build.describe());

            build
                .run_command(command, &mut |line| {
                    let _ = writeln!(log, "[build] {line}");
                })
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?;

        outcome?;
        self.events.emit(
            "release.built",
            &subject,
            "info",
            &format!(
                "{} in {:.1}s from uv.lock {digest}",
                environment.display(),
                started.elapsed().as_secs_f64()
            ),
        );
        Ok(python)
    }

    /// Section 8.3: the runtime library for an interpreter, from the
    /// environment, the state directory's runtime directories or a fetch,
    /// under the `[tools]` table's offline and wheel directory settings.
    async fn runtime_library(&self, python: &Path) -> Result<muru_provision::Provided, String> {
        let probe = muru_provision::probe::probe(python).map_err(|error| error.to_string())?;
        let tools = &self.info.provisioning.tools;
        let version = muru_provision::runtime::pep440(env!("CARGO_PKG_VERSION"));
        let root = self.locations.state.path.join("runtime");
        let wheel_dir = tools.wheel_dir.as_deref().map(PathBuf::from);
        let options = muru_provision::runtime::Options {
            probe: &probe,
            version: &version,
            root: &root,
            wheel_dir: wheel_dir.as_deref(),
            offline: tools.offline.unwrap_or(false),
        };

        muru_provision::runtime::provide(&options, "tools.wheel_dir")
            .await
            .map_err(|error| error.to_string())
    }

    /// Start a release's host and wait until every worker listens
    /// (pending or stopped to starting and warming; or failed). The
    /// caller marks the release ready once the edge has it, with
    /// `mark_ready`, so a client told it is ready can reach it.
    async fn start(&mut self, app: &str, id: &str) -> Result<(), String> {
        let release = self
            .store
            .release(app, id)
            .map_err(|error| error.to_string())?;
        let subject = format!("{app}@{id}");

        if let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned())) {
            // Retained and warm: no restart, and ready again once the
            // caller has given it to the edge.

            running.retained_until = None;
            return Ok(());
        }

        self.set_state(
            &release,
            ReleaseState::Starting,
            if release.spec.http.is_some() {
                "starting the server"
            } else {
                "starting the host"
            },
        );
        let spec = &release.spec;
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);

        let identity = match self.resolve_identity(app, &subject) {
            Ok(identity) => identity,
            Err(message) => {
                self.set_state(&release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.failed", &subject, "error", &message);
                return Err(message);
            }
        };

        // Section 10.8: secrets are resolved now, for this start, and
        // delivered as variables or as files only the host can read.

        let env = match resolve_env(&self.secrets, &spec.env, &run_dir.join("secrets")).await {
            Ok(env) => env,
            Err(message) => {
                self.set_state(&release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.failed", &subject, "error", &message);
                return Err(message);
            }
        };

        let started = Instant::now();

        // The release's directories, its secret files and its log belong
        // to the user the host runs as, so it can bind its sockets,
        // read its secrets and reopen its log.

        if let Some(identity) = identity
            && let Err(message) = self.own_release_dirs(identity, &run_dir, app, id)
        {
            self.set_state(&release, ReleaseState::Failed, &message);
            self.events
                .emit("release.failed", &subject, "error", &message);
            return Err(message);
        }

        // Section 9.9: a managed HTTP release runs its server's
        // instances instead of a host.

        if let Some(http) = spec.http.clone() {
            return self
                .start_servers(app, id, &release, &http, env, identity, &run_dir, started)
                .await;
        }

        // Section 8.2: the Building state, for a release whose mode and
        // source call for an environment of its own, built from the lock
        // as the release's user into the state directory. A failed build
        // fails the release before start-up, with uv's last line.

        let python = match self.build_environment(app, id, &release, identity).await {
            Ok(python) => python,
            Err(message) => {
                self.set_state(&release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.build_failed", &subject, "error", &message);
                return Err(message);
            }
        };

        // Section 8.3: the runtime library for the release's interpreter,
        // from the environment when it matches, else the state directory,
        // else fetched once. A mismatched copy that has to do is a warning
        // in the release's events; nothing usable fails the release.

        let library = match self.runtime_library(&python).await {
            Ok(library) => library,
            Err(message) => {
                self.set_state(&release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.failed", &subject, "error", &message);
                return Err(message);
            }
        };

        for note in &library.notes {
            let level = if library.how == muru_provision::How::Fallback {
                "warning"
            } else {
                "info"
            };
            self.events
                .emit("release.state_changed", &subject, level, note);
        }

        // Section 8.2: redirected bytecode goes to a directory of the
        // release's own, writable by its user and removed with it;
        // precompiled bytecode in the source is used where it is.

        let pycache_prefix = match spec.bytecode.effective() {
            muru_config::Bytecode::Redirect => {
                let prefix = crate::provision::Provisioning::pycache_dir(
                    &self.locations.state.path,
                    app,
                    id,
                );

                match std::fs::create_dir_all(&prefix) {
                    Ok(()) => {
                        if let Some(identity) = identity {
                            let _ = std::os::unix::fs::chown(
                                &prefix,
                                Some(identity.uid),
                                Some(identity.gid),
                            );
                        }

                        Some(prefix)
                    }
                    Err(error) => {
                        tracing::warn!(%error, "creating {} failed; bytecode is left to Python", prefix.display());
                        None
                    }
                }
            }
            _ => None,
        };

        let host_spec = HostSpec {
            python,
            pythonpath: library.pythonpath.clone(),
            pycache_prefix,
            no_bytecode: spec.bytecode.effective() == muru_config::Bytecode::Off,
            app: spec.entrypoint.clone(),
            name: Some(app.to_owned()),
            release: Some(id.to_owned()),
            interface: spec.interface,
            factory: spec.factory,
            reflection: spec.reflection,
            processes: spec.processes as usize,
            threads: spec.threads as usize,
            sync_threads: spec.sync_threads as usize,
            concurrency: spec.concurrency as usize,
            preload: spec.preload,
            hooks: spec.hooks.clone(),
            chdir: Some(spec.source.clone()),
            tcp_base_port: None,
            single: false,
            detach_to: Some(self.host_log(app, id)),
            logging: spec.logging.clone(),
            max_requests: spec.max_requests,
            max_lifetime: spec.max_lifetime_ms.map(Duration::from_millis),
            soft_timeout: spec.timeouts.soft_timeout_ms.map(Duration::from_millis),
            hard_timeout: spec.timeouts.hard_timeout_ms.map(Duration::from_millis),
            hard_drain_grace: Duration::from_millis(spec.timeouts.hard_drain_grace_ms),
            shutdown_timeout: Duration::from_millis(spec.timeouts.shutdown_timeout_ms),
            websocket_drain_grace: Duration::from_millis(spec.timeouts.websocket_drain_grace_ms),
            loop_stall_timeout: Duration::from_millis(spec.timeouts.loop_stall_timeout_ms),
            gil_stall_timeout: Duration::from_millis(spec.timeouts.gil_stall_timeout_ms),
            memory: spec.memory.clone(),
            env,
            identity,
            groups: spec
                .groups
                .iter()
                .map(|group| crate::host::HostGroup {
                    name: group.name.clone(),
                    processes: group.processes as usize,
                    threads: group.threads as usize,
                    sync_threads: group.sync_threads as usize,
                    concurrency: group.concurrency as usize,
                    timeouts: group.timeouts.resolve(&spec.timeouts),
                    memory: group.memory.clone().unwrap_or_else(|| spec.memory.clone()),
                    optional: group.optional,
                    mounts: group.mounts.clone(),
                })
                .collect(),
            mounts: spec
                .mounts
                .iter()
                .map(|mount| crate::host::HostMount {
                    name: mount.name.clone(),
                    path: mount.path.clone(),
                    entrypoint: mount.entrypoint.clone(),
                    interface: mount.interface,
                    threads: mount.threads,
                    concurrency: mount.concurrency.map(|value| value as usize),
                    max_connections: mount.max_connections.map(|value| value as usize),
                })
                .collect(),
        };

        let mut host = match HostProcess::start(&host_spec, &run_dir, 1, &self.logs) {
            Ok(host) => host,
            Err(error) => {
                let message = format!("the host could not be started: {error:#}");
                self.set_state(&release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.failed", &subject, "error", &message);
                return Err(message);
            }
        };

        self.set_state(&release, ReleaseState::Warming, "waiting for the workers");

        if let Err(error) = host.wait_ready(START_TIMEOUT).await {
            let message = format!("{error:#}");
            host.stop(release.spec.timeouts.stop_budget()).await;
            self.set_state(&release, ReleaseState::Failed, &message);
            self.events
                .emit("release.failed", &subject, "error", &message);
            return Err(message);
        }

        let sockets = host.default_sockets();
        let host_sockets = host.sockets.len();

        // ADR 0033: an optional group whose workers are not all listening
        // yet is named in the event; they join as the host announces them.

        let awaited: Vec<String> = spec
            .groups
            .iter()
            .filter(|group| group.optional)
            .filter(|group| {
                host.group_sockets
                    .iter()
                    .find(|(name, _)| name == &group.name)
                    .is_some_and(|(_, sockets)| sockets.len() < group.processes as usize)
            })
            .map(|group| format!("group {}", group.name))
            .collect();
        let groups: Vec<GroupRecord> = host
            .group_sockets
            .iter()
            .map(|(name, sockets)| {
                let group = spec.groups.iter().find(|group| &group.name == name);

                GroupRecord {
                    name: name.clone(),
                    paths: group.map(|group| group.paths.clone()).unwrap_or_default(),
                    client_classes: group
                        .map(|group| group.client_class.clone())
                        .unwrap_or_default(),
                    hosts: group.map(|group| group.host.clone()).unwrap_or_default(),
                    methods: group.map(|group| group.method.clone()).unwrap_or_default(),
                    optional: group.is_some_and(|group| group.optional),
                    sockets: sockets.clone(),
                }
            })
            .collect();
        write_record(
            &run_dir.join("host.toml"),
            &HostRecord {
                pid: host.pid().unwrap_or(0),
                sockets: host.sockets.clone(),
                groups: groups.clone(),
            },
        );
        let now = Instant::now();
        let pins = self.store.scale_pins(app, id).unwrap_or_default();
        let scalers = scalers_for(spec, &sockets, &groups, &pins, now);
        let ready_at = host
            .sockets
            .iter()
            .map(|socket| (socket.clone(), now))
            .collect();
        self.running.insert(
            (app.to_owned(), id.to_owned()),
            Running {
                backend: Backend::Host(host),
                sockets,
                groups,
                retained_until: None,
                sticky_until: None,
                held: false,
                control: None,
                status: Vec::new(),
                recycles: BTreeMap::new(),
                exits: BTreeMap::new(),
                signals_from: None,
                scalers,
                host_pss: None,
                idle_since: None,
                ready_at,
            },
        );
        self.attach_control(app, id);
        self.set_state(
            &release,
            ReleaseState::Warming,
            "workers ready, waiting for the edge",
        );
        self.events.emit(
            "release.ready",
            &subject,
            "info",
            &format!(
                "{} worker{} ready in {:.1} s{}",
                host_sockets,
                if host_sockets == 1 { "" } else { "s" },
                started.elapsed().as_secs_f64(),
                if awaited.is_empty() {
                    String::new()
                } else {
                    format!(", without the optional {}", awaited.join(", "))
                }
            ),
        );
        Ok(())
    }

    /// Section 10.9 and ADR 0026: whom a release runs as. Under a root
    /// daemon that is the application's user, or the service user when
    /// it names none, and never root unless the application says so,
    /// which is warned about; a daemon that is not root reports a
    /// configured user as not applied.
    fn resolve_identity(
        &self,
        app: &str,
        subject: &str,
    ) -> Result<Option<crate::host::Identity>, String> {
        let named = self.store.app(app).ok().and_then(|record| {
            record
                .user
                .clone()
                .map(|user| (user, record.group.clone(), record.umask.clone()))
        });

        match named {
            Some((user, group, umask)) if crate::host::Identity::can_switch() => {
                let mask = umask
                    .as_deref()
                    .and_then(|text| u32::from_str_radix(text, 8).ok());
                let identity = crate::host::Identity::lookup(&user, group.as_deref(), mask)
                    .map_err(|error| format!("the application's user cannot be used: {error:#}"))?;

                if identity.is_root() {
                    self.events.emit(
                        "app.user_is_root",
                        subject,
                        "warning",
                        &format!(
                            "user {user}: the release runs as root, which is not \
                             recommended; a compromised application would have the \
                             node"
                        ),
                    );
                }

                Ok(Some(identity))
            }
            Some((user, _, _)) => {
                self.events.emit(
                    "app.user_not_applied",
                    subject,
                    "warning",
                    &format!("user {user} not applied: the daemon is not root"),
                );
                Ok(None)
            }
            None => Ok(self.service),
        }
    }

    /// Give a release's directories, secret files and log to the user
    /// its processes run as, and open its socket directory to the edge's
    /// group alone (section 10.9), so the edge reaches the sockets
    /// without being root.
    fn own_release_dirs(
        &self,
        identity: crate::host::Identity,
        run_dir: &Path,
        app: &str,
        id: &str,
    ) -> Result<(), String> {
        let owned = [
            run_dir.to_path_buf(),
            run_dir.join("r1"),
            run_dir.join("secrets"),
        ];

        for path in &owned {
            let _ = std::fs::create_dir_all(path);
        }

        let mut paths: Vec<PathBuf> = owned.to_vec();

        if let Ok(entries) = std::fs::read_dir(run_dir.join("secrets")) {
            paths.extend(entries.flatten().map(|entry| entry.path()));
        }

        let log = self.host_log(app, id);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log);
        paths.push(log);

        for path in paths {
            identity.own(&path).map_err(|error| {
                format!(
                    "giving {} to the application's user failed: {error}",
                    path.display()
                )
            })?;
        }

        if let Some(service) = self.service {
            share_sockets(&run_dir.join("r1"), identity, service).map_err(|error| {
                format!("sharing the socket directory with the edge failed: {error}")
            })?;
        }

        Ok(())
    }

    /// Section 9.9: start a managed HTTP release's instances and wait
    /// for their readiness probes, then route to them as an upstream.
    #[allow(clippy::too_many_arguments)]
    async fn start_servers(
        &mut self,
        app: &str,
        id: &str,
        release: &Release,
        http: &crate::model::HttpServerSpec,
        env: Vec<(String, String)>,
        identity: Option<crate::host::Identity>,
        run_dir: &Path,
        started: Instant,
    ) -> Result<(), String> {
        let subject = format!("{app}@{id}");
        let log = self.host_log(app, id);
        let mut servers = match Servers::start(
            http,
            &run_dir.join("r1"),
            &release.spec.source,
            env,
            identity,
            &log,
        ) {
            Ok(servers) => servers,
            Err(error) => {
                let message = format!("the server could not be started: {error:#}");
                self.set_state(release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.failed", &subject, "error", &message);
                return Err(message);
            }
        };

        self.set_state(release, ReleaseState::Warming, "waiting for the instances");
        let timeout = Duration::from_millis(http.readiness_timeout_ms);

        if let Err(error) = servers.wait_ready(timeout).await {
            let message = format!("{error:#}");
            servers
                .stop(Duration::from_millis(
                    release.spec.timeouts.shutdown_timeout_ms,
                ))
                .await;
            self.set_state(release, ReleaseState::Failed, &message);
            self.events
                .emit("release.failed", &subject, "error", &message);
            return Err(message);
        }

        write_record(&run_dir.join("servers.toml"), &servers.record());
        self.running.insert(
            (app.to_owned(), id.to_owned()),
            Running {
                backend: Backend::Servers(servers),
                sockets: Vec::new(),
                groups: Vec::new(),
                retained_until: None,
                sticky_until: None,
                held: false,
                control: None,
                status: Vec::new(),
                recycles: BTreeMap::new(),
                exits: BTreeMap::new(),
                signals_from: None,
                scalers: Vec::new(),
                host_pss: None,
                idle_since: None,
                ready_at: HashMap::new(),
            },
        );
        self.set_state(
            release,
            ReleaseState::Warming,
            "instances ready, waiting for the edge",
        );
        self.events.emit(
            "release.ready",
            &subject,
            "info",
            &format!(
                "{} ready in {:.1} s",
                http.describe(),
                started.elapsed().as_secs_f64()
            ),
        );
        Ok(())
    }

    /// Point the edge at a release's workers: the application's serving
    /// release in the snapshot, and the default workers when no host is
    /// declared.
    async fn switch_to(&mut self, app: &str, id: &str) {
        let subject = format!("{app}@{id}");

        if !self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            return;
        }

        self.serving.insert(app.to_owned(), id.to_owned());
        self.default_app = Some(app.to_owned());

        if let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned())) {
            running.held = false;
        }

        let pushed = self.push_snapshot().await;
        self.mark_ready(app, id, "");

        match pushed {
            Ok(()) => {
                self.events.emit(
                    "traffic.switched",
                    &subject,
                    "notice",
                    "traffic moved to the release",
                );
            }
            Err(error) => {
                self.events.emit(
                    "release.failed",
                    &subject,
                    "error",
                    &format!("the edge could not take the release: {error}"),
                );
            }
        }
    }

    /// Section 4.4: desired state compiled for the edge. Each application
    /// contributes the workers of its serving release when that release
    /// runs; the hosts carry their certificates read from disk, so a
    /// replaced file takes effect with the next snapshot; the default
    /// workers are those of the application traffic was last switched to.
    fn build_snapshot(&mut self) -> pb::Snapshot {
        let mut apps = Vec::new();

        // Section 9.10: the routes and the applications in maintenance
        // mode with the active overlays laid over them.

        let (routes, maintained) = self.effective();
        let rollouts: Vec<Rollout> = self
            .store
            .rollouts(None)
            .unwrap_or_default()
            .into_iter()
            .filter(|rollout| rollout.state.is_active())
            .collect();
        let mut referenced = referenced_in(&routes);
        referenced.extend(
            rollouts
                .iter()
                .map(|rollout| (rollout.app.clone(), rollout.candidate.clone())),
        );

        // Section 9.4: an application's serving release takes requests,
        // and so do the releases its routes' policies name and a release
        // sticky clients may still reach, while they run and are not
        // merely kept warm.

        let mut routable: Vec<(&String, &String)> = self
            .running
            .iter()
            .filter(|((app, id), running)| {
                self.serving.get(app) == Some(id)
                    || running.sticky_until.is_some()
                    || running.held
                    || (running.retained_until.is_none()
                        && referenced.contains(&((*app).clone(), (*id).clone())))
            })
            .map(|((app, id), _)| (app, id))
            .collect();
        routable.sort();
        let included: BTreeSet<(String, String)> = routable
            .iter()
            .map(|(app, id)| ((*app).clone(), (*id).clone()))
            .collect();

        for (name, id) in routable {
            if let Some(running) = self.running.get(&(name.clone(), id.clone())) {
                apps.push(pb::AppWorkers {
                    app: name.clone(),
                    release: id.clone(),
                    serving: self.serving.get(name) == Some(id),
                    sockets: running
                        .sockets
                        .iter()
                        .map(|socket| socket.display().to_string())
                        .collect(),
                    waking_groups: running.waking_groups(),
                    grpc_web_origins: self
                        .store
                        .release(name, id)
                        .map(|release| release.spec.grpc_web_origins)
                        .unwrap_or_default(),
                    static_dirs: self
                        .store
                        .release(name, id)
                        .map(|release| {
                            release
                                .spec
                                .static_dirs
                                .iter()
                                .map(crate::model::StaticSpec::message)
                                .collect()
                        })
                        .unwrap_or_default(),
                    groups: running
                        .groups
                        .iter()
                        .map(|group| pb::Group {
                            name: group.name.clone(),
                            paths: group.paths.clone(),
                            client_classes: group.client_classes.clone(),
                            hosts: group.hosts.clone(),
                            methods: group.methods.clone(),
                            sockets: group
                                .sockets
                                .iter()
                                .map(|socket| socket.display().to_string())
                                .collect(),
                        })
                        .collect(),
                    mounts: self
                        .store
                        .release(name, id)
                        .map(|release| {
                            release
                                .spec
                                .mounts
                                .iter()
                                .map(|mount| pb::Mount {
                                    name: mount.name.clone(),
                                    path: mount.path.clone(),
                                    internal: mount.internal,
                                    accept_redirects_from: mount.accept_redirects_from.clone(),
                                    accel_redirect: mount.accel_redirect,
                                    cgi_local_redirect: mount.cgi_local_redirect,
                                    sendfile_roots: mount.sendfile_roots.clone(),
                                    sendfile_header: mount
                                        .sendfile_header
                                        .clone()
                                        .unwrap_or_default(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    accel_redirect: self
                        .store
                        .release(name, id)
                        .is_ok_and(|release| release.spec.accel_redirect),
                    cgi_local_redirect: self
                        .store
                        .release(name, id)
                        .is_ok_and(|release| release.spec.cgi_local_redirect),
                    sendfile_roots: self
                        .store
                        .release(name, id)
                        .map(|release| release.spec.sendfile_roots.clone())
                        .unwrap_or_default(),
                    sendfile_header: self
                        .store
                        .release(name, id)
                        .ok()
                        .and_then(|release| release.spec.sendfile_header.clone())
                        .unwrap_or_default(),
                    error_pages: self
                        .store
                        .release(name, id)
                        .map(|release| {
                            error_pages_message(
                                &release.spec.error_pages,
                                Some(&release.spec.source),
                            )
                        })
                        .unwrap_or_default(),
                    error_override: self
                        .store
                        .release(name, id)
                        .ok()
                        .and_then(|release| release.spec.error_override)
                        .map(|table| error_override_message(&table)),
                    maintenance: Some(maintenance_message(
                        maintained.contains(name),
                        self.store
                            .release(name, id)
                            .ok()
                            .and_then(|release| release.spec.maintenance)
                            .as_ref(),
                    )),
                    server: running.backend.server_message(name, id).map(|mut server| {
                        // Section 4.9: the release's file hint keys apply
                        // to the server's responses, which are trusted.

                        if let Ok(release) = self.store.release(name, id) {
                            server.sendfile_roots = release.spec.sendfile_roots.clone();
                            server.sendfile_header =
                                release.spec.sendfile_header.clone().unwrap_or_default();
                        }

                        server
                    }),
                });
            }
        }

        if self
            .default_app
            .as_ref()
            .is_some_and(|name| !apps.iter().any(|app| &app.app == name))
        {
            self.default_app = None;
        }
        let default = self
            .default_app
            .as_ref()
            .and_then(|name| apps.iter().find(|app| &app.app == name))
            .or_else(|| apps.first());
        let worker_sockets = default.map(|app| app.sockets.clone()).unwrap_or_default();
        let default_app = default.map(|app| app.app.clone()).unwrap_or_default();
        let default_release = default.map(|app| app.release.clone()).unwrap_or_default();

        // Section 4.2: a host applied since the last snapshot has its
        // certificate files read now; the others carry their last good
        // pair.

        self.refresh_certificates(false);
        let mut hosts = Vec::new();

        for (name, host) in self.store.hosts().unwrap_or_default() {
            let (certificate_pem, key_pem) =
                self.certificates.get(&name).cloned().unwrap_or_default();

            hosts.push(pb::Host {
                name,
                names: host.names,
                listeners: host.listeners,
                certificate_pem,
                key_pem,
                https_redirect: host.https_redirect,
                canonical: host.canonical.unwrap_or_default(),
                use_canonical_name: host.use_canonical_name,
                default: host.default,
                error_pages: error_pages_message(&host.error_pages, None),
                error_override: host.error_override.as_ref().map(error_override_message),
                rate_limits: host.rate_limits,
            });
        }

        let mut upstreams = Vec::new();

        for (name, upstream) in self.store.upstreams().unwrap_or_default() {
            match upstream_message(&name, &upstream) {
                Ok(message) => upstreams.push(message),
                Err(error) => self.events.emit(
                    "upstream.unusable",
                    &format!("upstream {name}"),
                    "error",
                    &format!("{error:#}; routes to it answer 502 until it is applied again"),
                ),
            }
        }

        let routes = routes
            .into_iter()
            .map(|route| pb::Route {
                upload: upload_message(&route),
                max_body_size: route
                    .max_body_size
                    .as_deref()
                    .and_then(|size| muru_config::parse_size(size).ok())
                    .unwrap_or(0),
                traffic: self.route_traffic(&route, &included, &rollouts),
                client_classes: route.classes().to_vec(),
                host: route.host,
                path: route.path,
                app: route.app,
                upstream: route.upstream,
                fallback_upstream: route
                    .fallback
                    .as_ref()
                    .and_then(|fallback| fallback.upstream.clone())
                    .unwrap_or_default(),
                static_dir: route.static_dir.unwrap_or_default(),
                index: route.index,
                fallback_app: route
                    .fallback
                    .as_ref()
                    .and_then(|fallback| fallback.app.clone())
                    .unwrap_or_default(),
                fallback_file: route
                    .fallback
                    .as_ref()
                    .and_then(|fallback| fallback.file.clone())
                    .unwrap_or_default(),
                cache_max_age_s: route
                    .cache
                    .as_ref()
                    .and_then(|cache| cache.max_age.as_deref())
                    .and_then(|text| muru_config::parse_duration(text).ok())
                    .map_or(0, |duration| duration.as_secs()),
                cache_immutable: route
                    .cache
                    .as_ref()
                    .and_then(|cache| cache.immutable)
                    .unwrap_or(false),
                cache_except: route
                    .cache
                    .as_ref()
                    .map(|cache| cache.except.clone())
                    .unwrap_or_default(),
                no_redirect_slash: route.redirect_slash == Some(false),
                case_insensitive: route.case.as_deref() == Some("insensitive"),
                internal: route.internal == Some(true),
                accept_redirects_from: route.accept_redirects_from,
                error_pages: error_pages_message(&route.error_pages, None),
                error_override: route.error_override.as_ref().map(error_override_message),
                auth: route.auth.as_ref().map(auth_message),
                rate_limits: route.rate_limits,
                methods: route.methods.as_ref().map(|methods| pb::Methods {
                    deny: methods.deny.clone(),
                    not_allowed: methods.answer.as_deref() == Some("method_not_allowed"),
                }),
                mode: route.mode.unwrap_or_default(),
            })
            .collect();

        // Section 9.3: with no host declared, the default application's
        // rollout chooses among its releases as a route's would.

        let default_traffic = if hosts.is_empty() && !default_app.is_empty() {
            let route = muru_config::Route {
                app: default_app.clone(),
                ..muru_config::Route::default()
            };
            self.route_traffic(&route, &included, &rollouts)
        } else {
            None
        };

        pb::Snapshot {
            version: 0,
            worker_sockets,
            hosts,
            routes,
            apps,
            proxy: self.proxy.clone(),
            compression: Some(self.compression.clone()),
            error_pages: self.error_pages.clone(),
            error_override: self.error_override.clone(),
            upstreams,
            default_app,
            default_release,
            client_classes: self.clients.classes_message.clone(),
            rate_limits: self.clients.limits_message.clone(),
            default_traffic,
            h2c: self.h2c.clone(),
            limits: Some(self.limits.to_message()),
        }
    }

    /// Compile and push a snapshot. A worker the edge could not connect to
    /// (section 5.1) is reported as an event and replaced at once, as
    /// `muructl restart --force` would: the snapshot has been applied
    /// without it, and a worker that accepts a connection and never
    /// answers is not going to.
    async fn push_snapshot(&mut self) -> Result<(), String> {
        let snapshot = self.build_snapshot();
        let applied = self
            .edge
            .apply(snapshot)
            .await
            .map_err(|error| format!("{error:#}"))?;
        self.replace_unreachable(applied);
        Ok(())
    }

    /// Section 5.1: report each worker the edge's answer to a snapshot
    /// names as one it could not connect to, and replace it at once.
    fn replace_unreachable(&mut self, applied: pb::Applied) {
        for worker in applied.unreachable {
            let subject = format!("{}@{}", worker.app, worker.release);
            let index = worker_index(&worker.socket);
            let whom = index.map_or_else(
                || format!("the worker at {}", worker.socket),
                |index| format!("worker {index}"),
            );
            let control = self
                .running
                .get(&(worker.app.clone(), worker.release.clone()))
                .and_then(|running| running.control.as_ref());

            let action = match (index, control) {
                (Some(index), Some(control)) => {
                    self.last_request += 1;
                    control.request(
                        self.last_request,
                        "restart",
                        Some(index),
                        &serde_json::json!({ "force": true }),
                    );
                    "; it is being replaced"
                }
                _ => "",
            };

            self.events.emit(
                "worker.unreachable",
                &subject,
                "warning",
                &format!(
                    "{whom} did not answer the edge ({}); the snapshot was applied without it{action}",
                    worker.reason
                ),
            );
        }
    }

    /// Section 9.5: with traffic already elsewhere, wait for the release's
    /// requests in flight to finish, then keep it warm for `retain` (section
    /// 9.1) or stop its host.
    async fn drain(&mut self, app: &str, id: &str, retain: Retention) {
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };

        if !self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            if release.state.is_running() {
                self.set_state(&release, ReleaseState::Stopped, "");
            }

            return;
        }

        // Section 9.4: with a sticky policy on the application's routes,
        // clients whose cookie names the release keep reaching it for the
        // grace period; it drains when that passes (expire_sticky).

        if let Some(grace) = self.sticky_grace(app)
            && let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned()))
            && running.sticky_until.is_none()
        {
            running.sticky_until = Some((Instant::now() + grace, retain));
            let message = format!(
                "sticky clients keep it for {}",
                muru_config::format_duration(grace)
            );
            self.set_state(&release, ReleaseState::Draining, &message);
            self.events
                .emit("release.draining", &subject, "info", &message);

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot for a sticky drain failed");
            }

            return;
        }

        self.drain_now(app, id, retain).await;
    }

    /// Drain a release that is out of the snapshot: wait for its requests
    /// in flight, then keep it warm for `retain` or stop it.
    async fn drain_now(&mut self, app: &str, id: &str, retain: Retention) {
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };

        // The previous pool is gone from the edge, so what is in flight on
        // these workers is what the edge had already dispatched; a short
        // wait lets it finish.

        self.set_state(
            &release,
            ReleaseState::Draining,
            "requests in flight are finishing",
        );
        self.events
            .emit("release.draining", &subject, "info", "draining");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // One retained release per application: an older one makes way.

        if !retain.lasts.is_zero() {
            let older: Vec<String> = self
                .running
                .iter()
                .filter(|((other_app, other_id), running)| {
                    other_app == app && other_id != id && running.retained_until.is_some()
                })
                .map(|((_, other_id), _)| other_id.clone())
                .collect();

            for other in older {
                self.stop_release(app, &other, "superseded again").await;
            }

            // Section 9.1: once drained, each group keeps its warm floor;
            // workers fork from the preloaded host when it serves again.

            let mut floors = Vec::new();

            if let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned())) {
                running.retained_until = Some(Instant::now() + retain.lasts);

                for group in &mut running.scalers {
                    let held = running
                        .groups
                        .iter()
                        .find(|record| record.name == group.name)
                        .map_or(running.sockets.len(), |record| record.sockets.len())
                        as u32;

                    if held > retain.workers && running.backend.host().is_some() {
                        group.requested = Some(retain.workers);
                        floors.push(group.name.clone());
                    }
                }
            }

            for group in floors {
                self.ask_scale(app, id, &group, retain.workers);
            }

            let until = crate::model::now_unix_ms() + retain.lasts.as_millis() as u64;
            self.set_state(
                &release,
                ReleaseState::Retained,
                &format!("warm until {}; a rollback to it is instant", clock(until)),
            );
            self.events.emit(
                "release.retained",
                &subject,
                "info",
                &format!(
                    "kept warm for {} with {} worker{} per group",
                    muru_config::format_duration(retain.lasts),
                    retain.workers,
                    if retain.workers == 1 { "" } else { "s" }
                ),
            );
            return;
        }

        self.stop_release(app, id, "").await;
    }

    /// Stop a release's host and record it stopped.
    async fn stop_release(&mut self, app: &str, id: &str, why: &str) {
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };
        let Some(running) = self.running.remove(&(app.to_owned(), id.to_owned())) else {
            return;
        };

        self.set_state(
            &release,
            ReleaseState::Stopping,
            if running.backend.servers().is_some() {
                "stopping the instances"
            } else {
                "stopping the host"
            },
        );
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);
        let record = running.backend.record_path(&run_dir);
        let grace = release.spec.timeouts.stop_budget();
        let stopped = tokio::time::timeout(stop_wait(grace), running.backend.stop(grace)).await;

        if stopped.is_err() {
            tracing::warn!(%subject, "the host did not stop within the drain timeout");
        }

        let _ = std::fs::remove_file(record);
        let _ = self.store.remove_scale_pins(app, id, None);
        self.set_state(&release, ReleaseState::Stopped, why);
        self.events
            .emit("release.stopped", &subject, "info", "stopped");
    }

    /// Section 9.3: begin a rollout of `candidate` against the release that
    /// serves. The candidate is started if it is not running, takes the
    /// first step's share at once, and the ticks move it along.
    async fn start_rollout(
        &mut self,
        app: &str,
        candidate: &str,
        steps: Vec<RolloutStep>,
        gates: RolloutGates,
        approve_each_step: bool,
    ) -> Result<Rollout, String> {
        let record = self.store.app(app).map_err(|error| error.to_string())?;
        let Some(baseline) = record.current.clone() else {
            return Err(format!("{app} has no serving release to roll out against"));
        };

        if baseline == candidate {
            return Err(format!("{app}@{candidate} already serves"));
        }

        self.store
            .release(app, candidate)
            .map_err(|error| error.to_string())?;

        if let Some(active) = self
            .store
            .active_rollout(app)
            .map_err(|error| error.to_string())?
        {
            return Err(format!(
                "{app} has rollout {} under way ({}); promote or abort it first",
                active.id,
                active.state.as_str()
            ));
        }

        if steps.is_empty() {
            return Err("a rollout needs at least one step".to_owned());
        }

        for url in &gates.probes {
            crate::rollouts::check_probe(url)?;
        }

        for (index, step) in steps.iter().enumerate() {
            if step.percent == 0 || step.percent > 100 {
                return Err(format!(
                    "step {}: the share must be 1 to 100 percent",
                    index + 1
                ));
            }

            if index > 0 && step.percent <= steps[index - 1].percent {
                return Err(format!("step {}: the shares must increase", index + 1));
            }
        }

        self.make_room_for(app, candidate).await?;
        self.start(app, candidate).await?;

        if let Some(running) = self
            .running
            .get_mut(&(app.to_owned(), candidate.to_owned()))
        {
            running.held = false;
        }

        let now = now_unix_ms();
        let rollout = Rollout {
            app: app.to_owned(),
            id: self
                .store
                .next_rollout_id(app)
                .map_err(|error| error.to_string())?,
            candidate: candidate.to_owned(),
            baseline: baseline.clone(),
            message: step_message(&steps, 0),
            steps,
            gates,
            approve_each_step,
            state: RolloutState::Running,
            step: 0,
            step_started_unix_ms: now,
            created_unix_ms: now,
            updated_unix_ms: now,
        };
        self.store
            .put_rollout(&rollout)
            .map_err(|error| error.to_string())?;
        self.events.emit(
            "rollout.started",
            &format!("{app}@{candidate}"),
            "notice",
            &format!(
                "rollout {} against {baseline}: {}",
                rollout.id,
                describe_steps(&rollout.steps)
            ),
        );
        let pushed = self.push_snapshot().await;
        self.mark_ready(app, candidate, "");
        pushed?;
        self.begin_rollout_run(&rollout).await;
        Ok(rollout)
    }

    /// Section 9.3: a release's counters for the gates, the edge's with
    /// its workers' crashes and timeout kills and its failed probes.
    fn counters(
        &self,
        samples: &[muru_edge::metrics::Sample],
        app: &str,
        release: &str,
    ) -> Counters {
        let key = (app.to_owned(), release.to_owned());
        let mut counters = Counters::from_samples(samples, app, release);
        counters.faults = self.worker_faults.get(&key).copied().unwrap_or(0) as f64;

        if let Some((failures, error)) = self
            .probe_failures
            .lock()
            .expect("probe failures poisoned")
            .get(&key)
        {
            counters.probe_failures = *failures as f64;
            counters.probe_error = Some(error.clone());
        }

        counters
    }

    /// Section 9.3: send a running rollout's probes to the candidate and
    /// the baseline, each through the edge by preview, when the interval
    /// has passed; a task sends them and records the round's failures
    /// together, so a fault both releases share is never judged half
    /// seen.
    fn send_probes(&mut self, rollout: &Rollout) {
        if rollout.gates.probes.is_empty()
            || self
                .probed
                .get(&rollout.app)
                .is_some_and(|sent| sent.elapsed() < crate::rollouts::PROBE_INTERVAL)
        {
            return;
        }

        self.probed.insert(rollout.app.clone(), Instant::now());
        let client = self.probe_client.clone();
        let failures = self.probe_failures.clone();
        let urls = rollout.gates.probes.clone();
        let app = rollout.app.clone();
        let releases = [rollout.candidate.clone(), rollout.baseline.clone()];

        tokio::spawn(async move {
            let mut failed = Vec::new();

            for release in releases {
                for url in &urls {
                    if let Err(error) = crate::rollouts::probe(&client, url, &release).await {
                        tracing::debug!(%app, %release, %error, "a rollout probe failed");
                        failed.push((release.clone(), error));
                    }
                }
            }

            let mut failures = failures.lock().expect("probe failures poisoned");

            for (release, error) in failed {
                let entry = failures
                    .entry((app.clone(), release))
                    .or_insert((0, String::new()));
                entry.0 += 1;
                entry.1 = error;
            }
        });
    }

    /// Section 9.3: mark where a rollout's current step starts from, once
    /// its share is in force, so the gates judge the requests served under
    /// this step alone. Without the edge's counters the first tick marks
    /// it instead.
    async fn begin_rollout_run(&mut self, rollout: &Rollout) {
        match self.edge.metrics().await {
            Ok(samples) => {
                self.rollout_runs.insert(
                    rollout.app.clone(),
                    RolloutRun {
                        rollout: rollout.id.clone(),
                        step: rollout.step,
                        candidate: self.counters(&samples, &rollout.app, &rollout.candidate),
                        baseline: self.counters(&samples, &rollout.app, &rollout.baseline),
                    },
                );
            }
            Err(error) => {
                tracing::debug!(%error, "the edge's metrics could not be read for the rollout");
                self.rollout_runs.remove(&rollout.app);
            }
        }
    }

    /// Pause, resume, promote or abort the application's active rollout.
    async fn control_rollout(
        &mut self,
        app: &str,
        action: RolloutAction,
    ) -> Result<Rollout, String> {
        let Some(mut rollout) = self
            .store
            .active_rollout(app)
            .map_err(|error| error.to_string())?
        else {
            return Err(format!("{app} has no rollout under way"));
        };

        let subject = format!("{app}@{}", rollout.candidate);
        let percent = rollout.percent();

        match action {
            RolloutAction::Pause => {
                if rollout.state != RolloutState::Running {
                    return Err(format!(
                        "rollout {} is {}",
                        rollout.id,
                        rollout.state.as_str()
                    ));
                }

                rollout.state = RolloutState::Paused;
                rollout.message = format!("paused at {percent}%");
                self.events
                    .emit("rollout.paused", &subject, "notice", &rollout.message);
            }
            RolloutAction::Resume => {
                let approved = match rollout.state {
                    RolloutState::Paused => false,
                    RolloutState::AwaitingApproval => true,
                    RolloutState::Running => {
                        return Err(format!("rollout {} is running", rollout.id));
                    }
                    RolloutState::Promoted | RolloutState::RolledBack => unreachable!(),
                };

                rollout.state = RolloutState::Running;
                rollout.step_started_unix_ms = now_unix_ms();
                rollout.message = step_message(&rollout.steps, rollout.step);

                if approved {
                    self.events.emit(
                        "rollout.step",
                        &subject,
                        "notice",
                        &format!("approved: {}", rollout.message),
                    );
                    self.store
                        .put_rollout(&rollout)
                        .map_err(|error| error.to_string())?;
                    self.push_snapshot().await?;
                } else {
                    self.events
                        .emit("rollout.resumed", &subject, "notice", &rollout.message);
                }

                self.begin_rollout_run(&rollout).await;
            }
            RolloutAction::Promote => {
                self.finish_rollout(
                    &mut rollout,
                    RolloutState::Promoted,
                    &format!("promoted by hand at {percent}%"),
                )
                .await?;
            }
            RolloutAction::Abort => {
                self.finish_rollout(
                    &mut rollout,
                    RolloutState::RolledBack,
                    &format!("aborted by hand at {percent}%"),
                )
                .await?;
            }
        }

        rollout.updated_unix_ms = now_unix_ms();
        self.store
            .put_rollout(&rollout)
            .map_err(|error| error.to_string())?;
        Ok(rollout)
    }

    /// End a rollout. Promoted, the candidate takes all the traffic and
    /// the baseline drains before the record says so; rolled back, the
    /// candidate leaves traffic and is kept warm, so it can be looked at.
    async fn finish_rollout(
        &mut self,
        rollout: &mut Rollout,
        state: RolloutState,
        message: &str,
    ) -> Result<(), String> {
        let subject = format!("{}@{}", rollout.app, rollout.candidate);
        self.rollout_runs.remove(&rollout.app);
        self.probed.remove(&rollout.app);

        let record = |rollout: &mut Rollout, store: &Store| -> Result<(), String> {
            rollout.state = state;
            rollout.message = message.to_owned();
            rollout.updated_unix_ms = now_unix_ms();
            store
                .put_rollout(rollout)
                .map_err(|error| error.to_string())
        };

        match state {
            RolloutState::Promoted => {
                self.promote_release(&rollout.app, &rollout.candidate)
                    .await?;
                record(rollout, &self.store)?;
                self.push_snapshot().await?;
                self.events.emit(
                    "rollout.completed",
                    &subject,
                    "notice",
                    &format!("rollout {}: {message}", rollout.id),
                );
            }
            _ => {
                record(rollout, &self.store)?;
                self.push_snapshot().await?;
                self.events.emit(
                    "rollout.rolled_back",
                    &subject,
                    "warning",
                    &format!(
                        "rollout {}: {message}; the candidate is kept warm",
                        rollout.id
                    ),
                );
                let candidate = (rollout.app.clone(), rollout.candidate.clone());

                if !self.referenced_releases().contains(&candidate)
                    && self.serving.get(&rollout.app) != Some(&rollout.candidate)
                {
                    self.drain(&rollout.app, &rollout.candidate, Retention::default())
                        .await;
                }
            }
        }

        Ok(())
    }

    /// Blue/green (section 9.2): all of the application's traffic to the
    /// release now, which is started if it is not running, and the release
    /// that served drains unless a policy names it.
    async fn promote_release(&mut self, app: &str, id: &str) -> Result<Release, String> {
        let record = self.store.app(app).map_err(|error| error.to_string())?;
        let current = record.current.clone();

        if current.as_deref() == Some(id) {
            return Err(format!("{app}@{id} already serves"));
        }

        self.make_room_for(app, id).await?;
        self.start(app, id).await?;
        self.switch_to(app, id).await;
        self.store
            .set_app_releases(app, Some(id), current.as_deref())
            .map_err(|error| error.to_string())?;

        if let Some(previous) = current.filter(|previous| previous != id)
            && !self
                .referenced_releases()
                .contains(&(app.to_owned(), previous.clone()))
        {
            self.drain(app, &previous, Retention::default()).await;
        }

        self.store
            .release(app, id)
            .map_err(|error| error.to_string())
    }

    /// Section 9.3: move every running rollout along. The current step is
    /// judged by its gates against the baseline over the same window; a
    /// failing gate rolls back at once; once the hold has passed with the
    /// gates satisfied, the next step begins, or waits for approval, or
    /// the last step promotes the candidate. Without enough requests to
    /// judge, the step holds until there are.
    async fn advance_rollouts(&mut self, samples: Option<&[muru_edge::metrics::Sample]>) {
        let Some(samples) = samples else {
            return;
        };
        let running: Vec<Rollout> = self
            .store
            .rollouts(None)
            .unwrap_or_default()
            .into_iter()
            .filter(|rollout| rollout.state == RolloutState::Running)
            .collect();

        // Section 9.10: an overlay that pins or splits a route's traffic
        // holds its application's rollout until it ends, as a deploy
        // freeze does (section 9.6).

        let pinned = {
            let overlays = self.store.overlays().unwrap_or_default();
            let states = self.store.overlay_states().unwrap_or_default();
            let active = crate::overlays::ordered(&overlays, &states);
            let mut pinned =
                crate::overlays::pinned_apps(&self.store.routes().unwrap_or_default(), &active);
            pinned.extend(crate::overlays::freezes(&active).into_keys());
            pinned
        };

        for mut rollout in running {
            let app = rollout.app.clone();

            if pinned.contains(&app) {
                continue;
            }

            let subject = format!("{app}@{}", rollout.candidate);
            let now = now_unix_ms();

            let run = match self.rollout_runs.get(&app) {
                Some(run) if run.rollout == rollout.id && run.step == rollout.step => run.clone(),
                _ => {
                    let run = RolloutRun {
                        rollout: rollout.id.clone(),
                        step: rollout.step,
                        candidate: self.counters(samples, &app, &rollout.candidate),
                        baseline: self.counters(samples, &app, &rollout.baseline),
                    };
                    rollout.step_started_unix_ms = now;
                    let _ = self.store.put_rollout(&rollout);
                    self.rollout_runs.insert(app.clone(), run.clone());
                    run
                }
            };

            self.send_probes(&rollout);
            let candidate = self
                .counters(samples, &app, &rollout.candidate)
                .since(&run.candidate);
            let baseline = self
                .counters(samples, &app, &rollout.baseline)
                .since(&run.baseline);
            let verdict = judge(&candidate, &baseline, &rollout.gates);
            let percent = rollout.percent();

            if let Verdict::Fail(reason) = verdict {
                self.events.emit(
                    "rollout.gate_failed",
                    &subject,
                    "warning",
                    &format!("step {} at {percent}%: {reason}", rollout.step + 1),
                );

                if let Err(error) = self
                    .finish_rollout(
                        &mut rollout,
                        RolloutState::RolledBack,
                        &format!("gate failed at {percent}%: {reason}"),
                    )
                    .await
                {
                    tracing::error!(%error, "rolling back a rollout failed");
                }

                continue;
            }

            let hold = rollout
                .steps
                .get(rollout.step)
                .map_or(0, |step| step.hold_ms);

            if now < rollout.step_started_unix_ms + hold {
                continue;
            }

            if verdict == Verdict::Insufficient {
                let message = format!(
                    "step {} at {percent}%: holding for {} requests to judge, {} so far",
                    rollout.step + 1,
                    rollout.gates.min_requests,
                    candidate.total as u64
                );

                if rollout.message != message {
                    rollout.message = message;
                    rollout.updated_unix_ms = now;
                    let _ = self.store.put_rollout(&rollout);
                }

                continue;
            }

            if rollout.step + 1 >= rollout.steps.len() {
                if let Err(error) = self
                    .finish_rollout(
                        &mut rollout,
                        RolloutState::Promoted,
                        "every step passed its gates",
                    )
                    .await
                {
                    tracing::error!(%error, "promoting a rollout failed");
                }

                continue;
            }

            rollout.step += 1;
            rollout.step_started_unix_ms = now;
            rollout.updated_unix_ms = now;
            self.rollout_runs.remove(&app);

            if rollout.approve_each_step {
                rollout.state = RolloutState::AwaitingApproval;
                rollout.message = format!(
                    "{} awaits approval: muructl rollout resume {app}",
                    step_message(&rollout.steps, rollout.step)
                );
                let _ = self.store.put_rollout(&rollout);
                self.events
                    .emit("rollout.step", &subject, "notice", &rollout.message);
                continue;
            }

            rollout.message = step_message(&rollout.steps, rollout.step);
            let _ = self.store.put_rollout(&rollout);
            self.events
                .emit("rollout.step", &subject, "notice", &rollout.message);

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot for a rollout step failed");
            }

            self.begin_rollout_run(&rollout).await;
        }
    }

    /// Section 9.4: the releases whose sticky grace has passed leave the
    /// snapshot and drain.
    async fn expire_sticky(&mut self) {
        let now = Instant::now();
        let expired: Vec<(String, String, Retention)> = self
            .running
            .iter()
            .filter_map(|((app, id), running)| {
                let (until, retain) = running.sticky_until?;
                (until <= now).then(|| (app.clone(), id.clone(), retain))
            })
            .collect();

        for (app, id, retain) in expired {
            if let Some(running) = self.running.get_mut(&(app.clone(), id.clone())) {
                running.sticky_until = None;
            }

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot after a sticky grace failed");
            }

            self.drain_now(&app, &id, retain).await;
        }
    }

    /// Section 9.4: the releases the route policies name must run, and a
    /// release that only a policy kept in traffic drains once no policy
    /// names it. Called after the configuration changes and at start.
    async fn reconcile_traffic(&mut self) {
        let mut referenced = self.referenced_releases();
        let warming: BTreeSet<(String, String)> = self
            .warm_ups
            .values()
            .flatten()
            .filter(|release| !referenced.contains(*release))
            .cloned()
            .collect();
        referenced.extend(warming.iter().cloned());
        let mut started = Vec::new();

        for (app, id) in &referenced {
            let key = (app.clone(), id.clone());

            let needs_start = match self.running.get(&key) {
                None => true,
                Some(running) => running.retained_until.is_some() || running.sticky_until.is_some(),
            };

            if !needs_start || self.store.release(app, id).is_err() {
                continue;
            }

            if let Some(running) = self.running.get_mut(&key) {
                running.sticky_until = None;
            }

            let started_now = match self.make_room_for(app, id).await {
                Ok(()) => self.start(app, id).await,
                Err(error) => Err(error),
            };

            match started_now {
                Ok(()) => started.push(key),
                Err(error) => {
                    let why = if warming.contains(&key) {
                        "an overlay's warm-up names the release"
                    } else {
                        "a traffic policy names the release"
                    };
                    self.events.emit(
                        "traffic_policy.release_unavailable",
                        &format!("{app}@{id}"),
                        "warning",
                        &format!("{why} but it cannot start: {error}"),
                    );
                }
            }
        }

        if !started.is_empty() {
            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot for a policy's releases failed");
            }

            for (app, id) in &started {
                self.mark_ready(app, id, "");
            }
        }

        let unreferenced: Vec<(String, String)> = self
            .running
            .iter()
            .filter(|((app, id), running)| {
                running.retained_until.is_none()
                    && running.sticky_until.is_none()
                    && !running.held
                    && self.serving.get(app) != Some(id)
                    && !referenced.contains(&((*app).clone(), (*id).clone()))
            })
            .map(|(key, _)| key.clone())
            .collect();

        for (app, id) in unreferenced {
            self.drain(&app, &id, Retention::default()).await;
        }
    }

    /// Section 7.4: feed every running release's scalers what the edge
    /// reports and carry out their decisions, each one an event with its
    /// reasons, scale-outs once the node's budget admits them. A group
    /// whose count is not what its scaler wants, after a restart or a
    /// restored pin, is asked for again.
    async fn autoscale(&mut self, samples: Option<&[muru_edge::metrics::Sample]>) {
        let Some(samples) = samples else {
            return;
        };
        let now = Instant::now();
        let mut requests: Vec<(String, String, String, u32)> = Vec::new();
        let mut emitted: Vec<(&'static str, String, &'static str, String)> = Vec::new();
        let mut capacities = self.capacities();

        // Scale-outs are admitted against what the releases hold before
        // any of them, the highest application priority first.

        let fallback = self.node_mean_pss();
        let used = self.budget_usage();
        let priorities: HashMap<String, i32> = self
            .store
            .apps()
            .unwrap_or_default()
            .into_iter()
            .map(|app| (app.name, app.priority))
            .collect();
        let mut asks = Vec::new();
        let mut reasons: HashMap<(String, String, String), (u32, String)> = HashMap::new();

        for ((app, id), running) in &mut self.running {
            // A retained release is at its warm floor (section 9.1), not
            // the autoscaler's to size.

            if running.retained_until.is_some() || running.control.is_none() {
                continue;
            }

            let subject = format!("{app}@{id}");

            for index in 0..running.scalers.len() {
                let cost = running.worker_cost(&running.scalers[index], fallback);
                let name = running.scalers[index].name.clone();
                let Some(sockets) = running.group_sockets(&name) else {
                    continue;
                };
                let ready = sockets.len() as u32;
                let youngest = sockets
                    .iter()
                    .filter_map(|socket| running.ready_at.get(socket))
                    .max()
                    .map(|at| now.saturating_duration_since(*at));
                let group = &mut running.scalers[index];
                let advisory = group.scaler.spec().advisory;
                group.scaler.set_capacity(capacities.remove(&(
                    app.clone(),
                    id.clone(),
                    group.name.clone(),
                )));

                if advisory {
                    group.scaler.sync(ready);
                }

                let before = group.scaler.desired();
                let signals = Signals::from_samples(
                    samples,
                    app,
                    id,
                    &group.name,
                    ready,
                    group.slots_per_worker,
                    youngest,
                );

                match group.scaler.observe(now, signals) {
                    Some(Decision::ScaleOut { to, reason }) if !advisory => {
                        let reason = format!(
                            "{reason}; {}",
                            describe_diagnosis(&group.scaler.diagnosis())
                        );
                        let key = (app.clone(), id.clone(), name);
                        reasons.insert(key.clone(), (ready, reason));
                        asks.push(crate::budget::Ask {
                            key,
                            priority: priorities.get(app).copied().unwrap_or(0),
                            from: before,
                            to,
                            cost,
                        });
                    }
                    Some(Decision::ScaleOut { to, reason } | Decision::ScaleIn { to, reason }) => {
                        let diagnosis = group.scaler.diagnosis();
                        emitted.push((
                            "scaling.decision",
                            subject.clone(),
                            "info",
                            decision_message(
                                &group.name,
                                ready,
                                to,
                                &format!("{reason}; {}", describe_diagnosis(&diagnosis)),
                                advisory,
                            ),
                        ));
                    }
                    Some(Decision::Suspended { reason }) => {
                        emitted.push((
                            "churn_budget.exhausted",
                            subject.clone(),
                            "warning",
                            format!("{}: {reason}", group.name),
                        ));
                    }
                    None => {}
                }
            }
        }

        for admitted in crate::budget::admit(&self.budget, used, asks) {
            let (app, id, name) = &admitted.ask.key;
            let Some(group) = self
                .running
                .get_mut(&(app.clone(), id.clone()))
                .and_then(|running| running.scalers.iter_mut().find(|group| &group.name == name))
            else {
                continue;
            };

            let subject = format!("{app}@{id}");
            let (ready, reason) = reasons.remove(&admitted.ask.key).unwrap_or_default();
            let from = admitted.ask.from;

            match admitted.limit {
                Some(limit) => {
                    let held = format!(
                        "the node's {limit} budget ({})",
                        self.budget.describe(&used)
                    );
                    group.scaler.limit(from, admitted.to, now, &held);
                    let denial = (admitted.ask.to, admitted.to, limit);

                    if group.denied != Some(denial) {
                        group.denied = Some(denial);
                        emitted.push((
                            "scaling.denied_by_budget",
                            subject.clone(),
                            "warning",
                            format!(
                                "{name}: {} of the {} more workers asked for fit {held}, at priority {}",
                                admitted.to - from,
                                admitted.ask.to - from,
                                admitted.ask.priority
                            ),
                        ));
                    }
                }
                None => group.denied = None,
            }

            if admitted.to > from {
                emitted.push((
                    "scaling.decision",
                    subject,
                    "info",
                    decision_message(name, ready, admitted.to, &reason, false),
                ));
            }
        }

        for ((app, id), running) in &mut self.running {
            if running.retained_until.is_some() || running.control.is_none() {
                continue;
            }

            for index in 0..running.scalers.len() {
                let Some(ready) = running
                    .group_sockets(&running.scalers[index].name)
                    .map(|sockets| sockets.len() as u32)
                else {
                    continue;
                };

                let group = &mut running.scalers[index];
                let desired = group.scaler.desired();

                if !group.scaler.spec().advisory
                    && desired != ready
                    && group.requested != Some(desired)
                {
                    group.requested = Some(desired);
                    requests.push((app.clone(), id.clone(), group.name.clone(), desired));
                }
            }
        }

        for (kind, subject, level, message) in emitted {
            self.events.emit(kind, &subject, level, &message);
        }

        for (app, id, group, processes) in requests {
            self.ask_scale(&app, &id, &group, processes);
        }

        // Section 9.1: over the memory budget, retained releases make way,
        // the oldest first, one a tick: down to the host alone, then
        // stopped.

        if self
            .budget
            .memory
            .is_some_and(|memory| used.memory > memory)
        {
            self.relieve_memory().await;
        }

        for (app, id, idle) in self.idle_hosts(now) {
            self.sleep_release(&app, &id, idle).await;
        }

        let view = self.budget_view(used);
        self.check_floors_ahead(&view);
        self.info
            .defined
            .write()
            .expect("defined names poisoned")
            .budget = view;
    }

    /// Section 7.5: each window of a schedule or overlay that raises a
    /// floor is checked against the node's budget once ahead of it, from
    /// the start of its warm-up's lead or `BUDGET_LEAD` before it,
    /// whichever is longer, with an event when it does not fit.
    fn check_floors_ahead(&mut self, view: &crate::budget::View) {
        if !self.budget.is_set() {
            return;
        }

        let now = now_unix_ms();
        let overlays = self.store.overlays().unwrap_or_default();

        for entry in &overlays {
            let (name, overlay) = entry;
            let Some((start, _)) = crate::overlays::window(overlay, now) else {
                continue;
            };
            let lead = overlay
                .warm_up
                .as_ref()
                .and_then(|warm_up| muru_config::parse_duration(&warm_up.lead).ok())
                .unwrap_or_default()
                .max(BUDGET_LEAD);

            if now >= start
                || start > now + lead.as_millis() as u64
                || !self.budget_checked.insert((name.clone(), start))
            {
                continue;
            }

            for ((app, group), capacity) in crate::overlays::capacities(std::iter::once(entry)) {
                if capacity.floor == 0 {
                    continue;
                }

                if let Err(why) = view.floor_fits(&app, &group, capacity.floor, None) {
                    self.events.emit(
                        "schedule.over_budget",
                        &format!("overlay {name}"),
                        "warning",
                        &format!(
                            "the window from {}: a floor of {} workers for {app} ({group} group) does not fit the node's budget as it stands: {why}",
                            clock(start),
                            capacity.floor
                        ),
                    );
                }
            }
        }

        self.budget_checked.retain(|(_, start)| *start > now);
    }

    /// Section 9.1: the node is over its memory budget. The retained
    /// release created first that still has workers drops to its host
    /// alone; when none has, the one created first stops.
    async fn relieve_memory(&mut self) {
        let mut retained: Vec<(u64, String, String)> = self
            .running
            .iter()
            .filter(|(_, running)| running.retained_until.is_some())
            .map(|((app, id), _)| {
                let created = self
                    .store
                    .release(app, id)
                    .map_or(0, |release| release.created_unix_ms);
                (created, app.clone(), id.clone())
            })
            .collect();
        retained.sort();

        for (_, app, id) in &retained {
            let key = (app.clone(), id.clone());
            let Some(running) = self.running.get_mut(&key) else {
                continue;
            };

            if running.workers() == 0 || running.backend.host().is_none() {
                continue;
            }

            let groups: Vec<String> = running
                .scalers
                .iter_mut()
                .map(|group| {
                    group.requested = Some(0);
                    group.name.clone()
                })
                .collect();

            for group in &groups {
                self.ask_scale(app, id, group, 0);
            }

            self.events.emit(
                "release.retained",
                &format!("{app}@{id}"),
                "warning",
                "down to its host alone: the node is over its memory budget",
            );
            return;
        }

        if let Some((_, app, id)) = retained.first() {
            self.stop_release(app, id, "the node is over its memory budget")
                .await;
        }
    }

    /// Section 7.4: the releases whose groups have all been at zero for
    /// their host_idle_timeout, with how long that is; the time each
    /// began is kept as it goes.
    fn idle_hosts(&mut self, now: Instant) -> Vec<(String, String, Duration)> {
        let mut idle = Vec::new();

        for ((app, id), running) in &mut self.running {
            let timeout = running
                .scalers
                .first()
                .and_then(|group| group.scaler.spec().host_idle_timeout_ms)
                .map(Duration::from_millis);

            let at_zero = running.backend.host().is_some()
                && running.control.is_some()
                && running.retained_until.is_none()
                && running.workers() == 0
                && running
                    .scalers
                    .iter()
                    .all(|group| group.scaler.desired() == 0);

            let Some(timeout) = timeout.filter(|_| at_zero) else {
                running.idle_since = None;
                continue;
            };

            let since = *running.idle_since.get_or_insert(now);

            if now.saturating_duration_since(since) >= timeout {
                idle.push((app.clone(), id.clone(), timeout));
            }
        }

        idle
    }

    /// Section 7.4: stop the host of a release whose groups have been at
    /// zero for its host_idle_timeout. The release keeps its place in
    /// the snapshot, so the next request waits at the edge and wakes it.
    async fn sleep_release(&mut self, app: &str, id: &str, idle: Duration) {
        let key = (app.to_owned(), id.to_owned());
        let Some(mut running) = self.running.remove(&key) else {
            return;
        };

        let backend = std::mem::replace(&mut running.backend, Backend::Dormant);
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);
        let record = backend.record_path(&run_dir);
        let grace = self
            .store
            .release(app, id)
            .map_or(Duration::from_secs(5), |release| {
                release.spec.timeouts.stop_budget()
            });

        if tokio::time::timeout(stop_wait(grace), backend.stop(grace))
            .await
            .is_err()
        {
            tracing::warn!(
                app,
                id,
                "the idle host did not stop within the drain timeout"
            );
        }

        let _ = std::fs::remove_file(record);
        running.control = None;
        running.status.clear();
        running.signals_from = None;
        running.host_pss = None;
        running.idle_since = None;
        self.running.insert(key, running);

        if let Ok(release) = self.store.release(app, id) {
            self.set_state(
                &release,
                ReleaseState::Ready,
                "idle: its host has stopped, and the next request starts it",
            );
        }

        self.events.emit(
            "release.idle",
            &format!("{app}@{id}"),
            "info",
            &format!(
                "every worker group has been at zero for {}: its host stopped, and the next request starts it again",
                muru_config::format_duration(idle)
            ),
        );
    }

    /// Section 7.4: a request is waiting at the edge for a group with no
    /// workers. A release whose host stopped while idle is started
    /// again; a group scaled to zero gets a worker, once the node's
    /// budget admits it.
    async fn wake(&mut self, wake: muru_proto::edge::v1::Wake) {
        let key = (wake.app.clone(), wake.release.clone());
        let subject = format!("{}@{}", wake.app, wake.release);
        let now = Instant::now();
        let fallback = self.node_mean_pss();

        let Some(running) = self.running.get_mut(&key) else {
            return;
        };

        if matches!(running.backend, Backend::Dormant) {
            self.running.remove(&key);
            self.events.emit(
                "release.woken",
                &subject,
                "info",
                "a request is waiting: starting its host again",
            );

            match self.start(&wake.app, &wake.release).await {
                Ok(()) => {
                    if let Err(error) = self.push_snapshot().await {
                        tracing::error!(%error, "pushing the snapshot for a woken release failed");
                    }

                    self.mark_ready(&wake.app, &wake.release, "woken by a request");
                }
                Err(error) => {
                    self.events
                        .emit("release.failed", &subject, "error", &error);
                }
            }

            return;
        }

        if running.retained_until.is_some() || running.control.is_none() {
            return;
        }

        let Some(index) = running
            .scalers
            .iter()
            .position(|group| group.name == wake.group)
        else {
            return;
        };

        let cost = running.worker_cost(&running.scalers[index], fallback);
        let group = &mut running.scalers[index];

        let Some(Decision::ScaleOut { to, reason }) = group.scaler.wake(now) else {
            return;
        };

        let priority = self
            .store
            .app(&wake.app)
            .map_or(0, |record| record.priority);
        let used = self.budget_usage();
        let ask = crate::budget::Ask {
            key: (),
            priority,
            from: 0,
            to,
            cost,
        };
        let admitted = crate::budget::admit(&self.budget, used, vec![ask]).remove(0);
        let Some(group) = self
            .running
            .get_mut(&key)
            .and_then(|running| running.scalers.get_mut(index))
        else {
            return;
        };

        if let Some(limit) = admitted.limit {
            let held = format!(
                "the node's {limit} budget ({})",
                self.budget.describe(&used)
            );
            group.scaler.limit(0, admitted.to, now, &held);
            self.events.emit(
                "scaling.denied_by_budget",
                &subject,
                "warning",
                &format!(
                    "{}: a request is waiting for a group scaled to zero, and no worker fits {held}",
                    wake.group
                ),
            );
            return;
        }

        group.requested = Some(to);
        self.events.emit(
            "scaling.decision",
            &subject,
            "info",
            &decision_message(&wake.group, 0, to, &reason, false),
        );
        self.ask_scale(&wake.app, &wake.release, &wake.group, to);
    }

    /// Section 7.4: the mean proportional set size of every worker that
    /// reported one, standing in for a group's that is not known.
    fn node_mean_pss(&self) -> Option<u64> {
        let sizes: Vec<u64> = self
            .running
            .values()
            .flat_map(|running| running.status.iter().filter_map(|worker| worker.pss))
            .collect();
        (!sizes.is_empty()).then(|| sizes.iter().sum::<u64>() / sizes.len() as u64)
    }

    /// Section 7.4: what the running releases hold against the node's
    /// budget.
    fn budget_usage(&self) -> crate::budget::Usage {
        let fallback = self.node_mean_pss();
        let mut used = crate::budget::Usage::default();

        for running in self.running.values() {
            used.add_usage(&running.usage(fallback));
        }

        used
    }

    /// Section 7.4: the node's budget and what its releases hold, for the
    /// checks before a release starts and on schedules.
    fn budget_view(&self, used: crate::budget::Usage) -> crate::budget::View {
        let fallback = self.node_mean_pss();
        let mut view = crate::budget::View {
            budget: self.budget,
            used,
            fallback,
            ..crate::budget::View::default()
        };

        for ((app, id), running) in &self.running {
            if running.retained_until.is_some() {
                view.retained += running.usage(fallback).memory;
                continue;
            }

            if self.serving.get(app) != Some(id) {
                continue;
            }

            if let Some(pss) = running.host_pss {
                view.hosts.insert(app.clone(), pss);
            }

            for group in &running.scalers {
                let held = running
                    .group_sockets(&group.name)
                    .map_or(0, <[PathBuf]>::len) as u32;
                view.groups.insert(
                    (app.clone(), group.name.clone()),
                    (
                        held.max(group.scaler.desired()),
                        running.worker_cost(group, None),
                    ),
                );
            }
        }

        view
    }

    /// Section 13.6: before a release starts, whether its memory fits the
    /// node's budget, retained releases making way, the oldest first,
    /// when that is enough; refused with a `scaling.denied_by_budget`
    /// event when it is not.
    async fn make_room(&mut self, app: &str, spec: &ReleaseSpec) -> Result<(), String> {
        if self.budget.memory.is_none() {
            return Ok(());
        }

        let view = self.budget_view(self.budget_usage());

        if let Err(why) = view.release_fits(app, spec, view.retained) {
            let message = if view.retained > 0 {
                format!("{app}: {why}, even with the retained releases stopped")
            } else {
                format!("{app}: {why}")
            };
            self.events
                .emit("scaling.denied_by_budget", app, "warning", &message);
            return Err(format!("not started: {message}"));
        }

        while self
            .budget_view(self.budget_usage())
            .release_fits(app, spec, 0)
            .is_err()
        {
            let Some((other_app, other_id)) = self.oldest_retained(None) else {
                break;
            };
            self.stop_release(
                &other_app,
                &other_id,
                &format!("making room in the node's memory budget for {app}"),
            )
            .await;
        }

        Ok(())
    }

    /// `make_room` for a recorded release that is not running.
    async fn make_room_for(&mut self, app: &str, id: &str) -> Result<(), String> {
        if self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            return Ok(());
        }

        let release = self
            .store
            .release(app, id)
            .map_err(|error| error.to_string())?;
        self.make_room(app, &release.spec).await
    }

    /// Section 9.1: the retained release created first, other than one
    /// of `except`'s application, which make way under memory pressure.
    fn oldest_retained(&self, except: Option<&str>) -> Option<(String, String)> {
        self.running
            .iter()
            .filter(|((app, _), running)| {
                running.retained_until.is_some() && except != Some(app.as_str())
            })
            .map(|((app, id), _)| {
                let created = self
                    .store
                    .release(app, id)
                    .map_or(0, |release| release.created_unix_ms);
                (created, app.clone(), id.clone())
            })
            .min()
            .map(|(_, app, id)| (app, id))
    }

    /// Section 7.5: the capacity active overlays and schedules set, and
    /// those warming up ahead of a window, for each running release's
    /// groups. A release taking a share of its application's traffic, as
    /// a canary does, takes that share of the floor.
    fn capacities(&self) -> HashMap<(String, String, String), crate::scaling::Capacity> {
        let overlays = self.store.overlays().unwrap_or_default();
        let states = self.store.overlay_states().unwrap_or_default();
        let mut applied = crate::overlays::ordered(&overlays, &states);
        applied.extend(
            overlays
                .iter()
                .filter(|(name, _)| self.warm_ups.contains_key(name)),
        );
        let by_app = crate::overlays::capacities(applied);
        let mut found = HashMap::new();

        if by_app.is_empty() {
            return found;
        }

        let routes = self.effective().0;
        let rollouts = self.store.rollouts(None).unwrap_or_default();

        for (app, id) in self.running.keys() {
            let share = self.traffic_share(app, id, &routes, &rollouts);

            if share == 0 {
                continue;
            }

            for ((capacity_app, group), capacity) in &by_app {
                if capacity_app != app {
                    continue;
                }

                let mut capacity = capacity.clone();
                capacity.floor = (capacity.floor * share).div_ceil(100);
                found.insert((app.clone(), id.clone(), group.clone()), capacity);
            }
        }

        found
    }

    /// The percentage of its application's requests a release takes: its
    /// rollout or policy share on the route that gives it the most, the
    /// rest for the release that serves, and none for another.
    fn traffic_share(
        &self,
        app: &str,
        id: &str,
        routes: &[muru_config::Route],
        rollouts: &[Rollout],
    ) -> u32 {
        let serves = self.serving.get(app).is_some_and(|serving| serving == id);
        let rollout = rollouts
            .iter()
            .find(|rollout| rollout.app == app && rollout.state.is_active());
        let mut best = None;

        for route in routes
            .iter()
            .filter(|route| route_apps(route).first() == Some(&app))
        {
            let mut shares: BTreeMap<&str, u32> = route
                .traffic
                .iter()
                .flat_map(|traffic| traffic.split.iter())
                .filter(|(release, _)| !release.starts_with(muru_edge::routing::UPSTREAM_PREFIX))
                .map(|(release, percent)| (release.as_str(), (*percent).min(100)))
                .collect();

            if let Some(rollout) = rollout {
                shares.insert(rollout.candidate.as_str(), rollout.percent().min(100));
            }

            let named: u32 = shares.values().sum();

            let share = match shares.get(id) {
                Some(share) => *share,
                None if serves => 100u32.saturating_sub(named),
                None => 0,
            };

            best = Some(best.unwrap_or(0).max(share));
        }

        // Without a route, a rollout of the default application shares
        // its requests all the same.

        match (best, rollout) {
            (Some(best), _) => best,
            (None, Some(rollout)) if rollout.candidate == id => rollout.percent().min(100),
            (None, Some(rollout)) if serves => 100u32.saturating_sub(rollout.percent().min(100)),
            (None, _) => {
                if serves {
                    100
                } else {
                    0
                }
            }
        }
    }

    /// Ask a release's host to bring a group to a worker count; the host
    /// announces the sockets as they change.
    fn ask_scale(&mut self, app: &str, id: &str, group: &str, processes: u32) {
        let Some(control) = self
            .running
            .get(&(app.to_owned(), id.to_owned()))
            .and_then(|running| running.control.as_ref())
        else {
            return;
        };

        self.last_request += 1;
        control.request(
            self.last_request,
            "scale",
            None,
            &serde_json::json!({ "group": group, "processes": processes }),
        );
    }

    /// Section 7.4: pin a group's worker count, so the autoscaler stands
    /// aside, or let it decide again; the pin outlives the daemon.
    fn set_scale(
        &mut self,
        app: &str,
        id: &str,
        group: &str,
        processes: Option<u32>,
        unpin: bool,
    ) -> Result<Vec<(String, ScaleStatus)>, String> {
        let now = Instant::now();
        let key = (app.to_owned(), id.to_owned());
        let Some(running) = self.running.get_mut(&key) else {
            return Err(format!("release {app}@{id} is not running"));
        };

        if running.backend.servers().is_some() {
            return Err(format!(
                "release {app}@{id} runs a managed HTTP server, whose instance count is fixed"
            ));
        }

        let Some(scaling) = running
            .scalers
            .iter_mut()
            .find(|scaling| scaling.name == group)
        else {
            return Err(format!("release {app}@{id} has no worker group {group}"));
        };

        let subject = format!("{app}@{id}");

        if unpin {
            if scaling.scaler.pinned().is_none() {
                return Err(format!("{group} is not pinned"));
            }

            scaling.scaler.unpin(now);
            self.store
                .remove_scale_pins(app, id, Some(group))
                .map_err(|error| error.to_string())?;
            self.events.emit(
                "scaling.decision",
                &subject,
                "notice",
                &format!(
                    "{group}: unpinned; the autoscaler decides again from {} workers",
                    scaling.scaler.desired()
                ),
            );
        } else {
            let Some(count) = processes else {
                return Err("give --processes COUNT or --unpin".to_owned());
            };
            let spec = scaling.scaler.spec();

            if count == 0 {
                return Err("processes must be at least 1".to_owned());
            }

            if !spec.is_static() && count > scaling.scaler.ceiling() {
                return Err(format!(
                    "{count} is over the group's max_workers of {}",
                    scaling.scaler.ceiling()
                ));
            }

            scaling.scaler.pin(count, now);
            scaling.requested = Some(count);
            self.store
                .put_scale_pin(app, id, group, count)
                .map_err(|error| error.to_string())?;
            self.events.emit(
                "scaling.decision",
                &subject,
                "notice",
                &format!("{group}: pinned to {count} workers by an operator"),
            );
            self.ask_scale(app, id, group, count);
        }

        self.scale_status(app, id)
    }

    /// Section 4.12: put an application into maintenance mode or take it
    /// out, recorded so a daemon restart keeps it, announced as an event,
    /// and pushed to the edge.
    async fn set_maintenance(&mut self, app: &str, on: bool) -> Result<Option<u64>, String> {
        self.store.app(app).map_err(|error| error.to_string())?;
        let was = self.maintenance.get(app).copied();

        match (on, was) {
            (true, Some(since)) => return Ok(Some(since)),
            (false, None) => return Ok(None),
            (true, None) => {
                let since = crate::model::now_unix_ms();
                self.store
                    .set_maintenance(app, Some(since))
                    .map_err(|error| error.to_string())?;
                self.maintenance.insert(app.to_owned(), since);
                self.events.emit(
                    "maintenance.changed",
                    app,
                    "notice",
                    "maintenance on: the edge serves the maintenance page",
                );
            }
            (false, Some(_)) => {
                self.store
                    .set_maintenance(app, None)
                    .map_err(|error| error.to_string())?;
                self.maintenance.remove(app);
                self.events.emit(
                    "maintenance.changed",
                    app,
                    "notice",
                    "maintenance off: the application serves again",
                );
            }
        }

        self.push_snapshot().await?;
        Ok(self.maintenance.get(app).copied())
    }

    /// The autoscaler's state for each group of a running release.
    fn scale_status(&self, app: &str, id: &str) -> Result<Vec<(String, ScaleStatus)>, String> {
        let now = Instant::now();
        let Some(running) = self.running.get(&(app.to_owned(), id.to_owned())) else {
            return Err(format!("release {app}@{id} is not running"));
        };

        // A retained release is not the autoscaler's while it is warm
        // (section 9.1): it holds the workers it has.

        Ok(running
            .scalers
            .iter()
            .map(|scaling| {
                let mut status = scaling.scaler.status(now);

                if running.retained_until.is_some() || running.control.is_none() {
                    status.ready = running
                        .group_sockets(&scaling.name)
                        .map_or(0, <[PathBuf]>::len) as u32;
                    status.desired = status.ready;
                }

                (scaling.name.clone(), status)
            })
            .collect())
    }

    /// Whether the edge routes to the release at all: it serves, keeps
    /// sticky clients, is held, or a policy or rollout refers to it.
    fn routable(&self, app: &str, id: &str) -> bool {
        let key = (app.to_owned(), id.to_owned());
        let Some(running) = self.running.get(&key) else {
            return false;
        };
        self.serving.get(app) == Some(&id.to_owned())
            || running.sticky_until.is_some()
            || running.held
            || (running.retained_until.is_none() && self.referenced_releases().contains(&key))
    }

    /// The releases the route policies name and the candidates of the
    /// rollouts under way, by application.
    fn referenced_releases(&self) -> BTreeSet<(String, String)> {
        let mut referenced = referenced_in(&self.effective().0);
        referenced.extend(
            self.store
                .rollouts(None)
                .unwrap_or_default()
                .into_iter()
                .filter(|rollout| rollout.state.is_active())
                .map(|rollout| (rollout.app, rollout.candidate)),
        );
        referenced
    }

    /// Section 9.4: the longest sticky grace among the application's
    /// routes with a sticky policy, or none when no route is sticky.
    fn sticky_grace(&self, app: &str) -> Option<Duration> {
        self.store
            .routes()
            .unwrap_or_default()
            .iter()
            .filter(|route| route_apps(route).contains(&app))
            .filter_map(|route| route.traffic.as_ref())
            .filter(|traffic| traffic.sticky == Some(true))
            .map(sticky_grace_of)
            .filter(|grace| !grace.is_zero())
            .max()
    }

    /// A route's traffic policy for the edge, when it has one or its
    /// application has a rollout under way: the rollout's candidate share
    /// first, then the policy's shares of the releases the snapshot
    /// carries, newest first so the newest release's buckets are at the
    /// top (section 9.4), with the key, salt and stickiness.
    fn route_traffic(
        &self,
        route: &muru_config::Route,
        included: &BTreeSet<(String, String)>,
        rollouts: &[Rollout],
    ) -> Option<pb::Traffic> {
        let app = route_apps(route).first().copied().unwrap_or_default();
        let rollout = rollouts.iter().find(|rollout| {
            rollout.app == app && included.contains(&(app.to_owned(), rollout.candidate.clone()))
        });

        if route.traffic.is_none() && rollout.is_none() {
            return None;
        }

        let mut shares: Vec<pb::Share> = rollout
            .map(|rollout| pb::Share {
                release: rollout.candidate.clone(),
                weight: rollout.percent().min(100) * 100,
                upstream: String::new(),
            })
            .into_iter()
            .collect();
        let traffic = route.traffic.clone().unwrap_or_default();
        let mut declared: Vec<(u64, &String, u32)> = traffic
            .split
            .iter()
            .filter(|(id, _)| !id.starts_with(muru_edge::routing::UPSTREAM_PREFIX))
            .filter(|(id, _)| included.contains(&(app.to_owned(), (*id).clone())))
            .filter(|(id, _)| rollout.is_none_or(|rollout| &rollout.candidate != *id))
            .map(|(id, percent)| {
                let created = self
                    .store
                    .release(app, id)
                    .map(|release| release.created_unix_ms)
                    .unwrap_or(0);
                (created, id, *percent)
            })
            .collect();
        declared.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));
        shares.extend(declared.into_iter().map(|(_, release, percent)| pb::Share {
            release: release.clone(),
            weight: percent.min(100) * 100,
            upstream: String::new(),
        }));

        // Section 4.13: shares to upstreams come after the releases', in
        // name order.

        shares.extend(traffic.split.iter().filter_map(|(id, percent)| {
            let upstream = id.strip_prefix(muru_edge::routing::UPSTREAM_PREFIX)?;
            Some(pb::Share {
                release: String::new(),
                weight: (*percent).min(100) * 100,
                upstream: upstream.to_owned(),
            })
        }));

        // Section 4.14: a class goes to its release while the snapshot
        // carries it, else as the policy's other requests do.

        let classes = traffic
            .classes
            .iter()
            .filter(|(_, id)| {
                id.starts_with(muru_edge::routing::UPSTREAM_PREFIX)
                    || included.contains(&(app.to_owned(), (*id).clone()))
            })
            .map(|(class, id)| (class.clone(), id.clone()))
            .collect();

        Some(pb::Traffic {
            shares,
            classes,
            hash_key: traffic.hash_key.clone().unwrap_or_default(),
            salt: traffic
                .salt
                .clone()
                .unwrap_or_else(|| format!("{} {}", route.host, route.path)),
            sticky: traffic.sticky.unwrap_or(false),
            sticky_grace_s: sticky_grace_of(&traffic).as_secs(),
        })
    }

    /// Stop the retained releases whose window has passed.
    async fn expire_retained(&mut self) {
        let now = Instant::now();
        let expired: Vec<(String, String)> = self
            .running
            .iter()
            .filter(|(_, running)| running.retained_until.is_some_and(|until| until <= now))
            .map(|(key, _)| key.clone())
            .collect();

        for (app, id) in expired {
            self.stop_release(&app, &id, "the retention window passed")
                .await;
        }
    }

    /// Section 9.6: start the previous release again if it is not running,
    /// move traffic to it, and drain the one that served.
    async fn rollback(&mut self, app: &str) -> Result<Release, String> {
        let record = self.store.app(app).map_err(|error| error.to_string())?;
        let Some(previous) = record.previous.clone() else {
            return Err(format!("{app} has no previous release to roll back to"));
        };
        let current = record.current.clone();

        self.start(app, &previous).await?;
        self.switch_to(app, &previous).await;
        self.store
            .set_app_releases(app, Some(&previous), current.as_deref())
            .map_err(|error| error.to_string())?;
        self.events.emit(
            "app.rolled-back",
            app,
            "notice",
            &format!("rolled back to {previous}"),
        );

        if let Some(current) = current.filter(|current| current != &previous)
            && !self
                .referenced_releases()
                .contains(&(app.to_owned(), current.clone()))
        {
            self.drain(app, &current, Retention::default()).await;
        }

        self.store
            .release(app, &previous)
            .map_err(|error| error.to_string())
    }

    /// Take a release out of traffic, if it serves, and stop it.
    async fn drain_release(&mut self, app: &str, id: &str) -> Result<Release, String> {
        let record = self.store.app(app).map_err(|error| error.to_string())?;
        self.store
            .release(app, id)
            .map_err(|error| error.to_string())?;

        if record.current.as_deref() == Some(id) {
            self.clear_traffic(app).await;
            self.store
                .set_app_releases(app, None, Some(id))
                .map_err(|error| error.to_string())?;
            self.events
                .emit("traffic.switched", app, "notice", "no release serves");
        }

        self.drain(
            app,
            id,
            Retention {
                lasts: Duration::ZERO,
                ..Retention::default()
            },
        )
        .await;

        self.store
            .release(app, id)
            .map_err(|error| error.to_string())
    }

    /// A host that exited on its own fails its release; the release that
    /// serves is started again once.
    async fn check_hosts(&mut self) {
        let mut exited = Vec::new();
        let mut reports = Vec::new();

        for (key, running) in &mut self.running {
            // Section 9.9: a managed server's instances are started
            // again where they crash; the release stays up.

            if let Backend::Servers(servers) = &mut running.backend {
                let changes = servers.check();

                if !changes.is_empty() {
                    let run_dir = self
                        .locations
                        .run
                        .path
                        .join("apps")
                        .join(&key.0)
                        .join(&key.1);
                    write_record(&run_dir.join("servers.toml"), &servers.record());
                }

                reports.extend(changes.into_iter().map(|report| (key.clone(), report)));
                continue;
            }

            if let Ok(Some(how)) = running.backend.try_exited() {
                exited.push((key.clone(), how));
            }
        }

        for ((app, id), report) in reports {
            self.events.emit(
                report.kind,
                &format!("{app}@{id}"),
                report.level,
                &report.message,
            );
        }

        for ((app, id), how) in exited {
            self.running.remove(&(app.clone(), id.clone()));
            let subject = format!("{app}@{id}");
            let message = format!("the host {how}");

            if let Ok(release) = self.store.release(&app, &id) {
                self.set_state(&release, ReleaseState::Failed, &message);
            }

            self.events
                .emit("release.failed", &subject, "error", &message);

            let serving = self
                .store
                .app(&app)
                .ok()
                .and_then(|record| record.current)
                .as_deref()
                == Some(id.as_str());

            if serving {
                self.clear_traffic(&app).await;
                self.events.emit(
                    "release.restarting",
                    &subject,
                    "warning",
                    "starting the release again",
                );

                if self.start(&app, &id).await.is_ok() {
                    self.switch_to(&app, &id).await;
                }
            }
        }
    }

    /// Push a snapshot without the application's workers: its serving
    /// release has just been unset or is being restarted.
    async fn clear_traffic(&mut self, app: &str) {
        self.serving.remove(app);

        if let Err(error) = self.push_snapshot().await {
            tracing::error!(%error, "pushing the snapshot failed");
        }
    }

    /// An edge that died is started again on the same listener with the
    /// last snapshot (section 2.4).
    async fn check_edge(&mut self) {
        match self.edge.check().await {
            Ok(None) => {}
            Ok(Some(applied)) => {
                self.info.edge_pid.store(
                    self.edge.pid().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                write_edge_record(&self.locations.run.path, &self.edge);
                self.events.emit(
                    "edge.restarted",
                    "edge",
                    "warning",
                    "the edge process exited and was started again on the same listener",
                );
                self.replace_unreachable(applied);
            }
            Err(error) => {
                self.events
                    .emit("edge.failed", "edge", "error", &format!("{error:#}"));
            }
        }
    }

    /// Section 5.9: connect to a running host's control socket in the
    /// background and feed what it says into the daemon's inbox.
    fn attach_control(&self, app: &str, id: &str) {
        let Some(host) = self
            .running
            .get(&(app.to_owned(), id.to_owned()))
            .and_then(|running| running.backend.host())
        else {
            return;
        };

        let path = host.control_path();
        let commands = self.commands.clone();
        let (app, id) = (app.to_owned(), id.to_owned());

        tokio::spawn(async move {
            let (control, mut messages) = match HostControl::connect(&path).await {
                Ok(connected) => connected,
                Err(error) => {
                    tracing::info!(%error, "release {app}@{id}: no host control connection");
                    return;
                }
            };

            if commands
                .send(Command::HostAttached {
                    app: app.clone(),
                    id: id.clone(),
                    control,
                })
                .await
                .is_err()
            {
                return;
            }

            while let Some(message) = messages.recv().await {
                let sent = commands
                    .send(Command::HostMessage {
                        app: app.clone(),
                        id: id.clone(),
                        message,
                    })
                    .await;

                if sent.is_err() {
                    break;
                }
            }
        });
    }

    /// Act on what a host said: new worker sockets go into the record
    /// and, when the release serves, into the next snapshot, and are
    /// acknowledged; events are recorded under the release.
    async fn host_message(&mut self, app: &str, id: &str, message: HostMessage) {
        let key = (app.to_owned(), id.to_owned());

        match message {
            HostMessage::Workers {
                version,
                sockets,
                groups,
                interfaces,
            } => {
                if !interfaces.is_empty() {
                    self.info
                        .interfaces
                        .lock()
                        .expect("interfaces poisoned")
                        .insert(key.clone(), interfaces);
                }

                let Some(running) = self.running.get_mut(&key) else {
                    return;
                };
                let Some((record, pid)) = running.backend.host().map(|host| {
                    (
                        host.run_dir().join("..").join("host.toml"),
                        host.pid().unwrap_or(0),
                    )
                }) else {
                    return;
                };
                let before: BTreeSet<PathBuf> = running.routed_sockets().into_iter().collect();
                running.sockets = sockets;

                for (name, group_sockets) in groups {
                    if let Some(group) = running.groups.iter_mut().find(|group| group.name == name)
                    {
                        group.sockets = group_sockets;
                    }
                }

                let all = running.routed_sockets();

                // Section 7.4: a socket new to the routing is a worker
                // that just became ready.

                let now = Instant::now();
                running.ready_at.retain(|socket, _| all.contains(socket));

                for socket in &all {
                    running.ready_at.entry(socket.clone()).or_insert(now);
                }

                // The host's first announcement on a control connection
                // repeats what the daemon read from the run directory when
                // the release started, just after a deploy answers. A push
                // for it would cost an edge apply and leave out a worker
                // that happens to be busy (section 5.1), for a routing that
                // is already in place, so one that changes nothing the edge
                // routes by is acknowledged without a push.

                let changed = before != all.iter().cloned().collect::<BTreeSet<PathBuf>>();
                write_record(
                    &record,
                    &HostRecord {
                        pid,
                        sockets: all,
                        groups: running.groups.clone(),
                    },
                );

                if changed
                    && self.routable(app, id)
                    && let Err(error) = self.push_snapshot().await
                {
                    tracing::error!(%error, "pushing the snapshot after a worker change failed");
                    return;
                }

                if let Some(control) = self
                    .running
                    .get(&key)
                    .and_then(|running| running.control.as_ref())
                {
                    control.acknowledge(version);
                }
            }
            HostMessage::Status { workers } => {
                if let Some(running) = self.running.get_mut(&key) {
                    // Section 7.3: the workers' own signals, from what
                    // grew since a report at least a few seconds older,
                    // since reports can arrive moments apart.

                    let now = Instant::now();

                    match &running.signals_from {
                        None => running.signals_from = Some((now, workers.clone())),
                        Some((at, _)) if now.saturating_duration_since(*at) < SIGNAL_WINDOW => {}
                        Some((at, before)) => {
                            let elapsed = now.saturating_duration_since(*at);

                            for group in &mut running.scalers {
                                let signals = crate::scaling::WorkerSignals::between(
                                    before,
                                    &workers,
                                    &group.name,
                                    elapsed,
                                    group.threads,
                                );
                                let near = group.memory_limit.is_some_and(|(measure, limit)| {
                                    workers
                                        .iter()
                                        .filter(|worker| worker.group == group.name)
                                        .filter_map(|worker| match measure {
                                            muru_config::Measure::Uss => worker.uss,
                                            muru_config::Measure::Pss => worker.pss,
                                            muru_config::Measure::Rss => worker.rss,
                                        })
                                        .any(|used| {
                                            used.saturating_mul(10) >= limit.saturating_mul(9)
                                        })
                                });
                                group.scaler.set_workers(signals, near);
                            }

                            running.signals_from = Some((now, workers.clone()));
                        }
                    }

                    // Section 7.4: the host is measured when its workers
                    // are, for the node's memory budget.

                    if workers.iter().any(|worker| worker.pss.is_some()) {
                        running.host_pss = running
                            .backend
                            .host()
                            .and_then(HostProcess::pid)
                            .and_then(crate::budget::process_pss);
                    }

                    running.status = workers;
                }
            }
            HostMessage::Response { id, workers, error } => {
                if let Some(done) = self.asked.remove(&id) {
                    let _ = done.send(if error.is_empty() {
                        Ok(workers)
                    } else {
                        Err(error)
                    });
                }
            }
            HostMessage::Event {
                kind,
                level,
                message,
                group,
                reason,
            } => {
                let level = match level.as_str() {
                    "error" => "error",
                    "warning" => "warning",
                    "notice" => "notice",
                    _ => "info",
                };

                // Section 11.1: a replaced worker and an exit are counted
                // by group and reason.

                if let Some(running) = self.running.get_mut(&key) {
                    let counts = match kind.as_str() {
                        "worker.recycled" | "worker.timeout_killed" => Some(&mut running.recycles),
                        "worker.exited" => Some(&mut running.exits),
                        _ => None,
                    };

                    if let Some(counts) = counts {
                        let group = if group.is_empty() {
                            muru_edge::routing::DEFAULT_GROUP.to_owned()
                        } else {
                            group
                        };

                        let reason = if reason.is_empty() {
                            "other".to_owned()
                        } else {
                            reason
                        };
                        *counts.entry((group, reason)).or_default() += 1;
                    }
                }

                // Section 9.3: a crash or a timeout kill counts against
                // the release in a rollout's gates.

                if kind == "worker.timeout_killed" || (kind == "worker.exited" && level != "info") {
                    *self
                        .worker_faults
                        .entry((app.to_owned(), id.to_owned()))
                        .or_default() += 1;
                }

                self.events
                    .emit(&kind, &format!("{app}@{id}"), level, &message);
            }
        }
    }

    /// Section 13.4: replace the edge with one from this daemon's image,
    /// on the same listeners or on `listeners` when a reload changed
    /// them (section 13.3); the old one finishes its connections and
    /// exits. `where_` says which in the event. Returns the old edge's
    /// pid; on failure the old edge keeps serving, and the event says so.
    async fn replace_edge(
        &mut self,
        listeners: Option<Vec<crate::edges::Listener>>,
        where_: &str,
    ) -> Result<u32, String> {
        let grace = self.drain_grace();

        match self.edge.replace(grace, listeners).await {
            Ok((old_pid, applied)) => {
                let pid = self.edge.pid().unwrap_or(0);
                self.info
                    .edge_pid
                    .store(pid, std::sync::atomic::Ordering::Relaxed);
                *self.info.listeners.write().expect("listeners poisoned") = self.edge.listeners();
                write_edge_record(&self.locations.run.path, &self.edge);
                self.events.emit(
                    "edge.replaced",
                    "edge",
                    "info",
                    &format!(
                        "edge pid {old_pid} replaced by pid {pid} on {where_}; the old edge finishes its connections and exits"
                    ),
                );
                self.replace_unreachable(applied);
                Ok(old_pid)
            }
            Err(error) => {
                let message = format!(
                    "the edge could not be replaced: {error:#}; the old edge keeps serving"
                );
                self.events.emit("edge.failed", "edge", "error", &message);
                Err(message)
            }
        }
    }

    /// Section 13.3: what a stop signal means to this daemon, said in the
    /// log; whether it shuts everything down.
    fn signalled(&self, signal: &str) -> bool {
        if self.shutdown.shuts_down() {
            tracing::info!("{signal}: shutting down ({})", self.shutdown.describe());
        } else {
            tracing::info!("{signal}: detaching; hosts and the edge keep serving");
        }

        self.shutdown.shuts_down()
    }

    /// Section 13.3: the edge's part of a shutdown. For the policy's
    /// delay it keeps accepting, with every connection closing after its
    /// response, then it stops accepting and the requests in flight
    /// finish, within what the budget leaves after a moment for the
    /// workers. The edge itself stays up for the WebSockets until it is
    /// stopped last.
    async fn drain_edge(&mut self, policy: &crate::shutdown::Policy, deadline: Instant) {
        let grace = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(policy.delay)
            .saturating_sub(crate::shutdown::WORKERS_RESERVE);

        match self.edge.drain(policy.delay, grace).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    "requests were still in flight when the drain's {} were up",
                    muru_config::format_duration(grace)
                );
            }
            Err(error) => {
                tracing::warn!(%error, "the edge could not be drained");
            }
        }
    }

    /// Stop every host, for shutdown: all at once, each given its workers'
    /// stop budget or what the shutdown's budget leaves, whichever ends
    /// first, and killed at `deadline` if it is still there.
    async fn stop_all_until(&mut self, deadline: Instant) {
        let running = std::mem::take(&mut self.running);
        self.serving.clear();

        if let Err(error) = self.push_snapshot().await {
            tracing::error!(%error, "pushing the empty snapshot failed");
        }

        let mut stopping = Vec::new();

        for ((app, id), running) in running {
            let release = self.store.release(&app, &id).ok();

            if let Some(release) = &release {
                self.set_state(release, ReleaseState::Stopping, "the daemon is stopping");
            }

            let budget = release
                .as_ref()
                .map(|release| release.spec.timeouts.stop_budget())
                .unwrap_or(DRAIN_TIMEOUT);
            let kill_at = deadline.min(Instant::now() + budget + crate::host::STOP_MARGIN);
            stopping.push(async move {
                running.backend.stop_until(kill_at).await;
                release
            });
        }

        for release in futures::future::join_all(stopping)
            .await
            .into_iter()
            .flatten()
        {
            self.set_state(&release, ReleaseState::Stopped, "the daemon stopped");
        }
    }

    /// Where a release's host writes its output, its directory created.
    fn host_log(&self, app: &str, id: &str) -> PathBuf {
        let path = self.locations.release_log(app, id);

        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        path
    }

    /// Section 11.3: after logrotate has renamed the files, every writer
    /// opens its file again by name. The daemon reopens its own when it
    /// was started in the background, the edge its access log on SIGUSR1,
    /// and each host its release log on SIGUSR1; SIGUSR2 makes a host
    /// pass SIGUSR1 on to its workers, whose on_reopen_logs hook handles
    /// files the application opened itself.
    /// Section 10.7: one point of history a second, from the edge's
    /// counters, and the WebSockets open by release for status.
    async fn sample_history(&mut self, samples: Option<&[muru_edge::metrics::Sample]>) {
        let Some(samples) = samples else {
            return;
        };

        let workers = self
            .running
            .values()
            .map(|running| {
                running.sockets.len()
                    + running
                        .groups
                        .iter()
                        .map(|group| group.sockets.len())
                        .sum::<usize>()
            })
            .sum::<usize>() as u64;
        let reading = crate::dashboard::reading(samples, workers);
        let now = crate::model::now_unix_ms() / 1000;
        let mut websockets: HashMap<(String, String), u64> = HashMap::new();

        for sample in samples
            .iter()
            .filter(|sample| sample.name == "muru_websocket_connections")
        {
            let label = |name: &str| {
                sample
                    .labels
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default()
            };
            *websockets
                .entry((label("app"), label("release")))
                .or_default() += sample.value.max(0.0) as u64;
        }

        *self.info.websockets.lock().expect("websockets poisoned") = websockets;

        if let Some(point) = self.sampler.observe(now, reading) {
            self.history.record(point);
        }
    }

    /// Section 11.3: rotate the log files that are due, by size or by the
    /// clock, then have every writer reopen its file by name.
    fn rotate_logs(&mut self) {
        let Some(rotator) = self.rotation.as_mut() else {
            return;
        };
        let now = std::time::SystemTime::now();
        let mut candidates = vec![
            (self.locations.access_log(), false),
            (self.locations.logs.path.join("edge.log"), false),
        ];

        if self.owns_log {
            candidates.push((self.locations.daemon_log(), false));
        }

        // Section 9.9: a managed server writes its log itself and cannot
        // be told to reopen it, so its log is rotated in place.

        for ((app, id), running) in &self.running {
            candidates.push((
                self.locations.release_log(app, id),
                running.backend.servers().is_some(),
            ));
        }

        let mut rotated = Vec::new();

        for (path, in_place) in candidates {
            if !path.is_file() || !rotator.due(&path, now) {
                continue;
            }

            let outcome = if in_place {
                rotator.rotate_in_place(&path, now)
            } else {
                rotator.rotate(&path, now)
            };

            match outcome {
                Ok(archive) => rotated.push(archive),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "rotating a log failed");
                }
            }
        }

        if rotated.is_empty() {
            return;
        }

        if self.owns_log {
            reopen_own_log(&self.locations.daemon_log());
        }

        self.edge.signal(libc::SIGUSR1);

        for running in self.running.values() {
            running.backend.signal(libc::SIGUSR1);
        }

        let names: Vec<String> = rotated
            .iter()
            .map(|archive| {
                archive
                    .strip_prefix(&self.locations.logs.path)
                    .unwrap_or(archive)
                    .display()
                    .to_string()
            })
            .collect();
        self.events.emit(
            "logs.rotated",
            "logs",
            "info",
            &format!("rotated {}", names.join(", ")),
        );
    }

    fn reopen_logs(&mut self, workers: bool) -> Reopened {
        if self.owns_log {
            reopen_own_log(&self.locations.daemon_log());
        }

        let edge = self.edge.signal(libc::SIGUSR1);
        let mut hosts = 0;

        for running in self.running.values() {
            if running.backend.signal(libc::SIGUSR1) {
                hosts += 1;
            }

            if workers {
                running.backend.signal(libc::SIGUSR2);
            }
        }

        let message = format!(
            "reopened the log files: {}{hosts} host{}{}",
            if edge { "the edge and " } else { "" },
            if hosts == 1 { "" } else { "s" },
            if workers { " and their workers" } else { "" }
        );
        self.events.emit("logs.reopened", "logs", "info", &message);
        Reopened { edge, hosts }
    }

    /// Mark a running release ready, once the snapshot that gives it to
    /// the edge has been pushed: the state means a request can reach it.
    /// A release that has meanwhile begun to stop is left as it is.
    fn mark_ready(&self, app: &str, id: &str, message: &str) {
        if !self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            return;
        }

        if let Ok(release) = self.store.release(app, id)
            && matches!(
                release.state,
                ReleaseState::Warming
                    | ReleaseState::Ready
                    | ReleaseState::Draining
                    | ReleaseState::Retained
            )
        {
            self.set_state(&release, ReleaseState::Ready, message);
        }
    }

    fn set_state(&self, release: &Release, state: ReleaseState, message: &str) {
        if let Err(error) = self
            .store
            .set_release_state(&release.app, &release.id, state, message)
        {
            tracing::error!(%error, "recording a release state failed");
        }
    }
}

/// A Unix time as HH:MM:SS, UTC.
pub(crate) fn clock_hms(unix_ms: u64) -> String {
    let seconds = (unix_ms / 1000) % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn clock(unix_ms: u64) -> String {
    let seconds = (unix_ms / 1000) % 86_400;
    format!("{:02}:{:02} UTC", seconds / 3600, (seconds % 3600) / 60)
}

/// Whether this process's standard output is the file at `path`, which
/// is how a daemon started in the background finds that it owns the
/// daemon log.
fn writes_to(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(current) = std::fs::metadata(path) else {
        return false;
    };

    // SAFETY: fstat on descriptor 1 of this process, into a zeroed struct
    // it fills; the descriptor stays open throughout.

    let (described, stat) = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        (libc::fstat(1, &mut stat) == 0, stat)
    };

    described
        && current.dev() as libc::dev_t == stat.st_dev
        && current.ino() as libc::ino_t == stat.st_ino
}

/// Open the daemon's log file again on standard output and error: once
/// logrotate has renamed it away, the lines belong in a new file at the
/// same path.
fn reopen_own_log(path: &std::path::Path) {
    use std::os::unix::io::AsRawFd as _;

    let Ok(log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };

    // SAFETY: dup2 of an open file onto the standard descriptors, which
    // stay valid; `log` closes its own descriptor when it drops.

    unsafe {
        libc::dup2(log.as_raw_fd(), 1);
        libc::dup2(log.as_raw_fd(), 2);
    }
}

/// Section 7.4: the scaling state of each group of a release that is
/// starting or adopted, from its spec, the workers it has and the pins
/// an operator left on it.
fn scalers_for(
    spec: &ReleaseSpec,
    sockets: &[PathBuf],
    groups: &[GroupRecord],
    pins: &[(String, u32)],
    now: Instant,
) -> Vec<GroupScaling> {
    // gRPC calls are held on the event loop too (section 6.13).

    let asgi = matches!(
        spec.interface,
        muru_config::Interface::Asgi | muru_config::Interface::Grpc
    );
    let slots = |threads: u32, concurrency: u32| {
        if asgi {
            concurrency as f64
        } else {
            threads as f64
        }
    };
    let threads = |threads: u32, sync_threads: u32| {
        if asgi { sync_threads + 1 } else { threads }
    };
    let memory_limit = spec
        .memory
        .soft_limit
        .map(|limit| (spec.memory.measure, limit));
    let mut scalers = vec![GroupScaling {
        name: muru_edge::routing::DEFAULT_GROUP.to_owned(),
        scaler: Scaler::new(spec.scaling.clone(), sockets.len() as u32),
        slots_per_worker: slots(spec.threads, spec.concurrency),
        requested: None,
        threads: threads(spec.threads, spec.sync_threads),
        memory_limit,
        denied: None,
    }];

    for group in &spec.groups {
        let current = groups
            .iter()
            .find(|record| record.name == group.name)
            .map_or(group.processes as usize, |record| record.sockets.len());
        scalers.push(GroupScaling {
            name: group.name.clone(),
            scaler: Scaler::new(group.scaling.clone(), current as u32),
            slots_per_worker: slots(group.threads, group.concurrency),
            requested: None,
            threads: threads(group.threads, group.sync_threads),
            memory_limit,
            denied: None,
        });
    }

    for (name, count) in pins {
        if let Some(scaling) = scalers.iter_mut().find(|scaling| &scaling.name == name) {
            scaling.scaler.pin(*count, now);
        }
    }

    scalers
}

/// Section 7.5: how long before a window raising a floor, at least, it
/// is checked against the node's budget.
const BUDGET_LEAD: Duration = Duration::from_secs(15 * 60);

/// How often the clock's synchronisation is checked.
const CLOCK_CHECK: Duration = Duration::from_secs(600);

/// Whether the system clock is synchronised, as the kernel's NTP state
/// says, or none where that cannot be told.
fn clock_synchronised() -> Option<bool> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // SAFETY: timex is plain data for which all zeroes is a valid
        // value, and with modes 0 the call only reads the clock's state
        // into it.

        let state = unsafe {
            let mut timex: libc::timex = std::mem::zeroed();

            #[cfg(target_os = "linux")]
            let state = libc::adjtimex(&mut timex);

            #[cfg(target_os = "macos")]
            let state = libc::ntp_adjtime(&mut timex);

            state
        };

        if state >= 0 {
            return Some(state != libc::TIME_ERROR);
        }
    }

    None
}

/// The event a clock check raises: a warning when the clock is not
/// synchronised while schedules depend on it, once, and a notice when it
/// is synchronised again after one.
fn clock_event(
    synchronised: Option<bool>,
    warned: bool,
    scheduled: bool,
) -> Option<(&'static str, &'static str, &'static str)> {
    match synchronised {
        Some(false) if scheduled && !warned => Some((
            "clock.unsynchronised",
            "warning",
            "the system clock is not synchronised; overlay schedules and scheduled capacity follow it",
        )),
        Some(true) if warned => Some((
            "clock.synchronised",
            "notice",
            "the system clock is synchronised again",
        )),
        _ => None,
    }
}

/// A scaling decision as its event says it.
/// Section 7.3: a diagnosis as words for an event or a status line.
fn describe_diagnosis(diagnosis: &crate::scaling::Diagnosis) -> String {
    if diagnosis.advice.is_empty() {
        format!("diagnosis: {}", diagnosis.summary)
    } else {
        format!(
            "diagnosis: {}; for the next release, {}",
            diagnosis.summary, diagnosis.advice
        )
    }
}

fn decision_message(group: &str, from: u32, to: u32, reason: &str, advisory: bool) -> String {
    let verb = if to > from { "scale out" } else { "scale in" };

    if advisory {
        format!("advisory: would {verb} {group} from {from} to {to} workers: {reason}")
    } else {
        format!("{group}: {verb} from {from} to {to} workers: {reason}")
    }
}

/// The applications a route sends requests to: the application, or a
/// static route's fallback.
fn route_apps(route: &muru_config::Route) -> Vec<&str> {
    let mut apps = Vec::new();

    if !route.app.is_empty() {
        apps.push(route.app.as_str());
    }

    if let Some(app) = route
        .fallback
        .as_ref()
        .and_then(|fallback| fallback.app.as_deref())
    {
        apps.push(app);
    }

    apps
}

/// The releases the routes' traffic policies name, as (application,
/// release) pairs (section 9.4), for a share or for a client class.
fn referenced_in(routes: &[muru_config::Route]) -> BTreeSet<(String, String)> {
    routes
        .iter()
        .filter_map(|route| route.traffic.as_ref().map(|traffic| (route, traffic)))
        .flat_map(|(route, traffic)| {
            route_apps(route).into_iter().flat_map(move |app| {
                traffic
                    .split
                    .keys()
                    .chain(traffic.classes.values())
                    .filter(|id| !id.starts_with(muru_edge::routing::UPSTREAM_PREFIX))
                    .map(move |id| (app.to_owned(), id.clone()))
            })
        })
        .collect()
}

/// Section 4.13: an upstream as the edge takes it, with its CA file read
/// now so a replaced file takes effect with the next snapshot.
fn upstream_message(name: &str, table: &muru_config::Upstream) -> anyhow::Result<pb::Upstream> {
    let millis = |text: Option<&str>| {
        text.and_then(|text| muru_config::parse_duration(text).ok())
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            })
    };

    let tls_ca_pem = match table.tls.as_ref().and_then(|tls| tls.ca.as_deref()) {
        Some(ca) => std::fs::read_to_string(ca).with_context(|| format!("reading {ca}"))?,
        None => String::new(),
    };

    let health = table.health.as_ref();

    Ok(pb::Upstream {
        name: name.to_owned(),
        addresses: table.addresses.clone(),
        protocol: table.protocol.clone().unwrap_or_default(),
        tls: table.tls.is_some(),
        tls_ca_pem,
        tls_server_name: table
            .tls
            .as_ref()
            .and_then(|tls| tls.server_name.clone())
            .unwrap_or_default(),
        tls_insecure: table
            .tls
            .as_ref()
            .is_some_and(|tls| tls.verify == Some(false)),
        trust_muru_headers: table.trust_muru_headers,
        sendfile_roots: table.sendfile_roots.clone(),
        sendfile_header: table.sendfile_header.clone().unwrap_or_default(),
        connect_timeout_ms: millis(table.connect_timeout.as_deref()),
        probe: health.map(|health| pb::UpstreamProbe {
            path: health.path.clone().unwrap_or_default(),
            interval_ms: millis(health.interval.as_deref()),
            timeout_ms: millis(health.timeout.as_deref()),
            unhealthy_after: health.unhealthy_after.unwrap_or(0),
            healthy_after: health.healthy_after.unwrap_or(0),
        }),
        eject_after: health.and_then(|health| health.eject_after).unwrap_or(3),
        eject_for_ms: millis(health.and_then(|health| health.eject_for.as_deref())),
    })
}

/// A policy's sticky grace: what it says, or 10 minutes.
fn sticky_grace_of(traffic: &muru_config::Traffic) -> Duration {
    traffic
        .sticky_grace
        .as_deref()
        .and_then(|text| muru_config::parse_duration(text).ok())
        .unwrap_or(STICKY_GRACE)
}

/// A rollout's steps in words: "10% for 2m, 50% for 2m".
fn describe_steps(steps: &[RolloutStep]) -> String {
    steps
        .iter()
        .map(|step| {
            format!(
                "{}% for {}",
                step.percent,
                muru_config::format_duration(Duration::from_millis(step.hold_ms))
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a rollout at this step is doing: "step 2 of 3: 50% for 2m".
fn step_message(steps: &[RolloutStep], step: usize) -> String {
    match steps.get(step) {
        Some(current) => format!(
            "step {} of {}: {}% for {}",
            step + 1,
            steps.len(),
            current.percent,
            muru_config::format_duration(Duration::from_millis(current.hold_ms))
        ),
        None => format!("step {} of {}", step + 1, steps.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_warning_is_raised_once_and_only_for_schedules() {
        assert_eq!(
            clock_event(Some(false), false, true).map(|(kind, ..)| kind),
            Some("clock.unsynchronised")
        );
        assert_eq!(clock_event(Some(false), true, true), None);
        assert_eq!(clock_event(Some(false), false, false), None);
        assert_eq!(
            clock_event(Some(true), true, true).map(|(kind, ..)| kind),
            Some("clock.synchronised")
        );
        assert_eq!(clock_event(Some(true), false, true), None);
        assert_eq!(clock_event(None, false, true), None);
    }

    #[test]
    fn the_clock_state_can_be_read_here() {
        assert!(clock_synchronised().is_some());
    }
}
