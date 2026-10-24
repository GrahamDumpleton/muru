//! Virtual hosts and routes (design specification sections 4.3 to 4.5):
//! the tables a snapshot compiles into, consulted once per request.
//!
//! Without any host the edge serves one application, as in development
//! mode: every request goes to its workers. With hosts, a request's host
//! name selects the virtual host (exact name, then the wildcard with the
//! longest suffix, then the listener's default host, else 421), the
//! longest route prefix selects the application, and the application's
//! serving release takes the request. Within a release, the most specific
//! worker group path selects the group's workers, else the default
//! group's (section 7.1).

use std::collections::HashMap;
use std::sync::Arc;

use muru_proto::edge::v1 as pb;

use crate::link::LinkPool;
use crate::proxy::Trust;

/// What the snapshot says about one virtual host.
#[derive(Debug, Clone)]
pub struct Host {
    /// The host's name in the configuration.
    pub name: String,
    /// The listener addresses it answers on; every listener when empty.
    pub listeners: Vec<String>,
    /// Plain HTTP requests are redirected to HTTPS.
    pub https_redirect: bool,
    /// The name the application sees as SERVER_NAME.
    pub server_name: String,
}

struct Route {
    path: String,
    app: String,
    /// The directory served, for a static route (section 4.9).
    static_route: Option<Arc<crate::static_files::StaticRoute>>,
    /// The application that takes what no file answers.
    fallback_app: String,
}

/// The worker sockets of one group of a release, as a snapshot or a
/// development run names them.
#[derive(Debug, Clone)]
pub struct GroupSockets {
    /// The group's name.
    pub name: String,
    /// The paths it serves: prefixes, with an optional trailing `*`.
    pub paths: Vec<String>,
    /// Its workers.
    pub sockets: Vec<std::path::PathBuf>,
}

/// A named mount of a release, as a snapshot or a development run names it.
#[derive(Debug, Clone)]
pub struct MountPath {
    /// The mount's name, as the request head carries it.
    pub name: String,
    /// The path prefix it serves.
    pub path: String,
}

/// Where a request goes within an application: the group's pool and the
/// mount, with the prefix the application sees as SCRIPT_NAME.
pub struct Selection {
    /// The workers.
    pub pool: Arc<LinkPool>,
    /// The application's name, for the access log; empty when unnamed.
    pub app: String,
    /// The release, for the access log; empty when unnamed.
    pub release: String,
    /// The worker group's name: `default`, or a named group's.
    pub group: String,
    /// The mount's name, empty for the default mount.
    pub mount: String,
    /// The mount's path, empty for the default mount.
    pub script_name: String,
}

/// An application's connected workers: the default group and the named
/// groups, each with the paths that select it, most specific first, and
/// the mounts, longest prefix first.
pub struct AppPools {
    app: String,
    release: String,
    default: Arc<LinkPool>,
    /// A group path prefix, the group's name and its pool.
    groups: Vec<(String, String, Arc<LinkPool>)>,
    mounts: Vec<MountPath>,
}

impl AppPools {
    /// Connect the default workers and each group's. `app` and `release`
    /// label the requests served in the access log.
    pub async fn connect(
        app: &str,
        release: &str,
        default: &[std::path::PathBuf],
        groups: &[GroupSockets],
        mounts: &[MountPath],
    ) -> anyhow::Result<Self> {
        let default = Arc::new(LinkPool::connect(default).await?);
        let mut connected = Vec::new();

        for group in groups {
            let pool = Arc::new(LinkPool::connect(&group.sockets).await?);

            for path in &group.paths {
                connected.push((group_prefix(path), group.name.clone(), Arc::clone(&pool)));
            }
        }

        connected.sort_by_key(|(path, _, _)| std::cmp::Reverse(path.len()));
        let mut mounts: Vec<MountPath> = mounts
            .iter()
            .map(|mount| MountPath {
                name: mount.name.clone(),
                path: group_prefix(&mount.path),
            })
            .collect();
        mounts.sort_by_key(|mount| std::cmp::Reverse(mount.path.len()));

        Ok(Self {
            app: app.to_owned(),
            release: release.to_owned(),
            default,
            groups: connected,
            mounts,
        })
    }

    /// A single pool with no groups or mounts, and no labels.
    pub fn single(pool: LinkPool) -> Self {
        Self {
            app: String::new(),
            release: String::new(),
            default: Arc::new(pool),
            groups: Vec::new(),
            mounts: Vec::new(),
        }
    }

    /// Where a path goes: the most specific group path's pool, else the
    /// default group's, and the longest mount prefix, else the default
    /// mount (section 6.7).
    pub fn select(&self, path: &str) -> Selection {
        let (group, pool) = self
            .groups
            .iter()
            .find(|(prefix, _, _)| prefix_matches(prefix, path))
            .map(|(_, name, pool)| (name.clone(), Arc::clone(pool)))
            .unwrap_or_else(|| (DEFAULT_GROUP.to_owned(), Arc::clone(&self.default)));
        let mount = self
            .mounts
            .iter()
            .find(|mount| prefix_matches(&mount.path, path));

        Selection {
            pool,
            app: self.app.clone(),
            release: self.release.clone(),
            group,
            mount: mount.map(|mount| mount.name.clone()).unwrap_or_default(),
            script_name: mount.map(|mount| mount.path.clone()).unwrap_or_default(),
        }
    }

    /// The default group's pool.
    pub fn default_pool(&self) -> Arc<LinkPool> {
        Arc::clone(&self.default)
    }

    /// The application and release the pools serve, empty when nothing
    /// names them.
    pub fn labels(&self) -> (&str, &str) {
        (&self.app, &self.release)
    }

    /// Every group's pool once, by group name, the default group first.
    pub fn pools(&self) -> Vec<(String, Arc<LinkPool>)> {
        let mut pools = vec![(DEFAULT_GROUP.to_owned(), Arc::clone(&self.default))];

        for (_, name, pool) in &self.groups {
            if !pools.iter().any(|(seen, _)| seen == name) {
                pools.push((name.clone(), Arc::clone(pool)));
            }
        }

        pools
    }
}

/// The name of the group a release's own settings size (section 7.1).
pub const DEFAULT_GROUP: &str = "default";

/// A group path as a route-style prefix: a trailing `*` means anything
/// below, which is what a prefix already means.
fn group_prefix(path: &str) -> String {
    let trimmed = path.trim_end_matches('*');

    let trimmed = if trimmed.len() > 1 {
        trimmed.trim_end_matches('/')
    } else {
        trimmed
    };

    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The compiled routing tables of one snapshot.
pub struct Routing {
    version: u64,
    trust: Trust,
    compression: crate::compression::Compression,
    default: Arc<AppPools>,
    apps: HashMap<String, Arc<AppPools>>,
    hosts: Vec<Host>,
    exact: HashMap<String, usize>,
    wildcards: Vec<(String, usize)>,
    default_host: Option<usize>,
    routes: Vec<Vec<Route>>,
}

/// Where a request goes.
pub enum Decision {
    /// To this pool and mount, with the name the application sees as
    /// SERVER_NAME.
    Serve {
        /// The group's workers and the mount.
        selection: Selection,
        /// The matched or canonical host name, when a host matched.
        server_name: Option<String>,
    },
    /// Redirected to HTTPS: the host insists on it.
    Redirect,
    /// No virtual host answers to that name on this listener (421).
    Misdirected,
    /// The host has no route for the path (404).
    NoRoute,
    /// To a directory of files (section 4.9), with the application that
    /// takes what no file answers, when the route names one.
    Static {
        /// The route's directory and settings.
        route: Arc<crate::static_files::StaticRoute>,
        /// The route's path prefix.
        prefix: String,
        /// The fallback application's workers, when there is one.
        fallback: Option<Selection>,
        /// The matched or canonical host name.
        server_name: Option<String>,
    },
}

impl Routing {
    /// One application and nothing else: development mode, or a daemon
    /// with no host declared.
    pub fn single(pools: AppPools) -> Self {
        Self {
            version: 0,
            trust: Trust::default(),
            compression: crate::compression::Compression::default(),
            default: Arc::new(pools),
            apps: HashMap::new(),
            hosts: Vec::new(),
            exact: HashMap::new(),
            wildcards: Vec::new(),
            default_host: None,
            routes: Vec::new(),
        }
    }

    /// Compile a snapshot, connecting to every worker it names first, so
    /// a snapshot that cannot be reached is refused whole.
    pub async fn from_snapshot(snapshot: &pb::Snapshot) -> anyhow::Result<Self> {
        let mut apps = HashMap::new();

        for app in &snapshot.apps {
            let pools = AppPools::connect(
                &app.app,
                &app.release,
                &paths(&app.sockets),
                &groups_of(app),
                &mounts_of(app),
            )
            .await?;
            apps.insert(app.app.clone(), Arc::new(pools));
        }

        // The default workers are one application's, found by its sockets
        // so its groups come along and its pools are shared; without one,
        // a plain pool.

        let default_app = snapshot
            .apps
            .iter()
            .find(|app| app.sockets == snapshot.worker_sockets)
            .and_then(|app| apps.get(&app.app));

        let default = match default_app {
            Some(pools) => Arc::clone(pools),
            None => Arc::new(AppPools::single(
                LinkPool::connect(&paths(&snapshot.worker_sockets)).await?,
            )),
        };

        // Exact names go in a map; wildcards keep their suffix, longest
        // first, so *.shop.example.com beats *.example.com.

        let trust = match &snapshot.proxy {
            Some(proxy) => Trust::from_snapshot(proxy).map_err(|error| anyhow::anyhow!(error))?,
            None => Trust::default(),
        };

        let compression = match &snapshot.compression {
            Some(message) => crate::compression::Compression::from_snapshot(message)
                .map_err(|error| anyhow::anyhow!("compression: {error}"))?,
            None => crate::compression::Compression::default(),
        };

        let mut routing = Self {
            version: snapshot.version,
            trust,
            compression,
            default,
            apps,
            hosts: Vec::new(),
            exact: HashMap::new(),
            wildcards: Vec::new(),
            default_host: None,
            routes: Vec::new(),
        };

        for host in &snapshot.hosts {
            let index = routing.hosts.len();

            for name in &host.names {
                let name = normalise(name);

                match name.strip_prefix("*.") {
                    Some(suffix) => routing.wildcards.push((format!(".{suffix}"), index)),
                    None => {
                        routing.exact.insert(name, index);
                    }
                }
            }

            if host.default && routing.default_host.is_none() {
                routing.default_host = Some(index);
            }

            let server_name = if host.use_canonical_name && !host.canonical.is_empty() {
                host.canonical.clone()
            } else {
                String::new()
            };
            routing.hosts.push(Host {
                name: host.name.clone(),
                listeners: host.listeners.clone(),
                https_redirect: host.https_redirect,
                server_name,
            });
            routing.routes.push(Vec::new());
        }

        routing
            .wildcards
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));

        // Routes are kept per host, longest prefix first.

        let by_name: HashMap<&str, usize> = routing
            .hosts
            .iter()
            .enumerate()
            .map(|(index, host)| (host.name.as_str(), index))
            .collect();

        for route in &snapshot.routes {
            if let Some(&index) = by_name.get(route.host.as_str()) {
                let static_route = crate::static_files::StaticRoute::from_message(route)
                    .map_err(|error| {
                        anyhow::anyhow!("route {} {}: {error}", route.host, route.path)
                    })?
                    .map(Arc::new);
                routing.routes[index].push(Route {
                    path: route.path.clone(),
                    app: route.app.clone(),
                    static_route,
                    fallback_app: route.fallback_app.clone(),
                });
            }
        }

        for routes in &mut routing.routes {
            routes.sort_by_key(|route| std::cmp::Reverse(route.path.len()));
        }

        Ok(routing)
    }

    /// The snapshot's version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Which peers may speak for the client (section 4.7).
    pub fn trust(&self) -> &Trust {
        &self.trust
    }

    /// How responses are compressed (section 4.17).
    pub fn compression(&self) -> &crate::compression::Compression {
        &self.compression
    }

    /// The same tables with this compression policy, for a development
    /// run, which has no snapshot to carry it.
    pub fn with_compression(mut self, compression: crate::compression::Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Whether any virtual host is declared.
    pub fn has_hosts(&self) -> bool {
        !self.hosts.is_empty()
    }

    /// The default application's default pool, which is what the
    /// dashboard counts.
    pub fn default_pool(&self) -> Arc<LinkPool> {
        self.default.default_pool()
    }

    /// Every application's pools once: the named applications, and the
    /// default pools when they are nobody's.
    pub fn app_pools(&self) -> Vec<Arc<AppPools>> {
        let mut pools: Vec<Arc<AppPools>> = self.apps.values().cloned().collect();
        pools.sort_by(|left, right| left.labels().cmp(&right.labels()));

        if !pools.iter().any(|seen| Arc::ptr_eq(seen, &self.default)) {
            pools.insert(0, Arc::clone(&self.default));
        }

        pools
    }

    /// Where a request goes, from the host the client asked for, the
    /// path, the listener it arrived on and whether that was over TLS.
    pub fn select(&self, name: Option<&str>, path: &str, listener: &str, tls: bool) -> Decision {
        if self.hosts.is_empty() {
            return Decision::Serve {
                selection: self.default.select(path),
                server_name: None,
            };
        }

        let Some(index) = self.host_for(name, listener) else {
            return Decision::Misdirected;
        };
        let host = &self.hosts[index];

        if host.https_redirect && !tls {
            return Decision::Redirect;
        }

        let Some(route) = self.routes[index]
            .iter()
            .find(|route| prefix_matches(&route.path, path))
        else {
            return Decision::NoRoute;
        };

        let select_app = |app: &str| {
            self.apps
                .get(app)
                .map(|pools| pools.select(path))
                .unwrap_or_else(|| Selection {
                    pool: Arc::new(LinkPool::empty()),
                    app: app.to_owned(),
                    release: String::new(),
                    group: DEFAULT_GROUP.to_owned(),
                    mount: String::new(),
                    script_name: String::new(),
                })
        };

        if let Some(static_route) = &route.static_route {
            return Decision::Static {
                route: Arc::clone(static_route),
                prefix: route.path.clone(),
                fallback: (!route.fallback_app.is_empty()).then(|| select_app(&route.fallback_app)),
                server_name: Some(if host.server_name.is_empty() {
                    name.map(normalise).unwrap_or_default()
                } else {
                    host.server_name.clone()
                }),
            };
        }

        let selection = select_app(&route.app);

        // SERVER_NAME is the name the request matched with, or the
        // canonical name when the host says so (section 4.3).

        let server_name = if host.server_name.is_empty() {
            name.map(normalise)
        } else {
            Some(host.server_name.clone())
        };

        Decision::Serve {
            selection,
            server_name,
        }
    }

    /// The host a name selects on a listener: exact, wildcard, default.
    fn host_for(&self, host: Option<&str>, listener: &str) -> Option<usize> {
        let answers = |index: &usize| {
            let host = &self.hosts[*index];
            host.listeners.is_empty() || host.listeners.iter().any(|address| address == listener)
        };
        let name = host.map(normalise);

        if let Some(name) = &name {
            if let Some(index) = self.exact.get(name).filter(|index| answers(index)) {
                return Some(*index);
            }

            for (suffix, index) in &self.wildcards {
                if wildcard_matches(suffix, name) && answers(index) {
                    return Some(*index);
                }
            }
        }

        self.default_host.filter(answers)
    }
}

fn paths(sockets: &[String]) -> Vec<std::path::PathBuf> {
    sockets.iter().map(std::path::PathBuf::from).collect()
}

fn mounts_of(app: &pb::AppWorkers) -> Vec<MountPath> {
    app.mounts
        .iter()
        .map(|mount| MountPath {
            name: mount.name.clone(),
            path: mount.path.clone(),
        })
        .collect()
}

fn groups_of(app: &pb::AppWorkers) -> Vec<GroupSockets> {
    app.groups
        .iter()
        .map(|group| GroupSockets {
            name: group.name.clone(),
            paths: group.paths.clone(),
            sockets: paths(&group.sockets),
        })
        .collect()
}

/// A host name as the tables hold it: lower case, no port, no trailing dot.
pub fn normalise(host: &str) -> String {
    let mut name = host_only(host).to_ascii_lowercase();

    while name.ends_with('.') {
        name.pop();
    }

    name
}

/// The host part of a Host header: the port is removed, brackets around
/// an IPv6 address are kept.
pub(crate) fn host_only(host: &str) -> String {
    let host = host.trim();

    if host.starts_with('[') {
        return match host.find(']') {
            Some(end) => host[..=end].to_owned(),
            None => host.to_owned(),
        };
    }

    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
            name.to_owned()
        }
        _ => host.to_owned(),
    }
}

/// Whether `*.suffix` matches a name: one label before the suffix.
fn wildcard_matches(suffix: &str, name: &str) -> bool {
    match name.strip_suffix(suffix) {
        Some(label) => !label.is_empty() && !label.contains('.'),
        None => false,
    }
}

/// Whether a route's path prefix covers a request path, on a segment
/// boundary: /api covers /api and /api/users but not /apiary.
fn prefix_matches(prefix: &str, path: &str) -> bool {
    if prefix == "/" || prefix == path {
        return true;
    }

    let prefix = prefix.trim_end_matches('/');

    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> pb::Snapshot {
        pb::Snapshot {
            version: 7,
            compression: None,
            worker_sockets: Vec::new(),
            hosts: vec![
                pb::Host {
                    name: "shop".to_owned(),
                    names: vec![
                        "Shop.example.com.".to_owned(),
                        "*.shop.example.com".to_owned(),
                    ],
                    https_redirect: true,
                    canonical: "shop.example.com".to_owned(),
                    use_canonical_name: true,
                    ..Default::default()
                },
                pb::Host {
                    name: "any".to_owned(),
                    names: vec!["*.example.com".to_owned()],
                    listeners: vec!["127.0.0.1:8080".to_owned()],
                    default: true,
                    ..Default::default()
                },
            ],
            routes: vec![
                pb::Route {
                    host: "shop".to_owned(),
                    path: "/".to_owned(),
                    app: "shop".to_owned(),
                    ..Default::default()
                },
                pb::Route {
                    host: "shop".to_owned(),
                    path: "/api".to_owned(),
                    app: "api".to_owned(),
                    ..Default::default()
                },
                pb::Route {
                    host: "any".to_owned(),
                    path: "/".to_owned(),
                    app: "shop".to_owned(),
                    ..Default::default()
                },
            ],
            apps: Vec::new(),
            proxy: None,
        }
    }

    fn routing() -> Routing {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(Routing::from_snapshot(&snapshot()))
            .unwrap()
    }

    fn serves(decision: Decision) -> Option<String> {
        match decision {
            Decision::Serve { server_name, .. } => server_name,
            Decision::Redirect => Some("redirect".to_owned()),
            Decision::Misdirected => Some("421".to_owned()),
            Decision::NoRoute => Some("404".to_owned()),
            Decision::Static { server_name, .. } => server_name,
        }
    }

    #[test]
    fn names_select_hosts_in_the_order_of_section_4_3() {
        let routing = routing();
        let plain = "127.0.0.1:8080";

        assert_eq!(
            serves(routing.select(Some("shop.example.com:8443"), "/", plain, true)),
            Some("shop.example.com".to_owned()),
            "exact, case and port aside, with the canonical name"
        );
        assert_eq!(
            serves(routing.select(Some("eu.shop.example.com"), "/", plain, true)),
            Some("shop.example.com".to_owned()),
            "the longest wildcard suffix wins"
        );
        assert_eq!(
            serves(routing.select(Some("Blog.example.com"), "/", plain, false)),
            Some("blog.example.com".to_owned()),
            "a wildcard match gives the name asked for"
        );
        assert_eq!(
            serves(routing.select(Some("a.b.example.com"), "/", plain, false)),
            Some("a.b.example.com".to_owned()),
            "two labels do not match a single-label wildcard, so the default host answers"
        );
        assert_eq!(
            serves(routing.select(Some("a.b.example.com"), "/", "127.0.0.1:8443", true)),
            Some("421".to_owned()),
            "the default host answers only on its listener"
        );
        assert_eq!(
            serves(routing.select(None, "/", plain, false)),
            None,
            "no host header falls to the default host"
        );
    }

    #[test]
    fn redirects_and_routes() {
        let routing = routing();
        let plain = "127.0.0.1:8080";

        assert_eq!(
            serves(routing.select(Some("shop.example.com"), "/", plain, false)),
            Some("redirect".to_owned())
        );

        match routing.select(Some("shop.example.com"), "/api/users", plain, true) {
            Decision::Serve { selection, .. } => {
                assert!(selection.pool.is_empty(), "the api app has no workers");
            }
            _ => panic!("not served"),
        }

        assert!(prefix_matches("/api", "/api"));
        assert!(prefix_matches("/api", "/api/users"));
        assert!(!prefix_matches("/api", "/apiary"));
        assert!(prefix_matches("/api/", "/api/users"));
        assert!(prefix_matches("/", "/anything"));
    }

    #[test]
    fn without_hosts_everything_is_served() {
        let routing = Routing::single(AppPools::single(LinkPool::empty()));
        assert!(!routing.has_hosts());
        assert!(matches!(
            routing.select(Some("whatever"), "/", "127.0.0.1:1", false),
            Decision::Serve { .. }
        ));
    }

    #[test]
    fn group_paths_become_prefixes() {
        assert_eq!(group_prefix("/reports/*"), "/reports");
        assert_eq!(group_prefix("/api/status"), "/api/status");
        assert_eq!(group_prefix("/*"), "/");
        assert_eq!(group_prefix("*"), "/");
    }

    #[test]
    fn host_only_strips_ports_and_keeps_ipv6_brackets() {
        assert_eq!(host_only("shop.localhost:8080"), "shop.localhost");
        assert_eq!(host_only("localhost"), "localhost");
        assert_eq!(host_only("[::1]:8080"), "[::1]");
        assert_eq!(host_only("[::1]"), "[::1]");
        assert_eq!(host_only("127.0.0.1:8080 "), "127.0.0.1");
        assert_eq!(normalise("Shop.Example.COM."), "shop.example.com");
    }
}
