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

/// An application's connected workers: the default group and the named
/// groups, each with the paths that select it, most specific first.
pub struct AppPools {
    default: Arc<LinkPool>,
    groups: Vec<(String, Arc<LinkPool>)>,
}

impl AppPools {
    /// Connect the default workers and each group's.
    pub async fn connect(
        default: &[std::path::PathBuf],
        groups: &[GroupSockets],
    ) -> anyhow::Result<Self> {
        let default = Arc::new(LinkPool::connect(default).await?);
        let mut connected = Vec::new();

        for group in groups {
            let pool = Arc::new(LinkPool::connect(&group.sockets).await?);

            for path in &group.paths {
                connected.push((group_prefix(path), Arc::clone(&pool)));
            }
        }

        connected.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));

        Ok(Self {
            default,
            groups: connected,
        })
    }

    /// A single pool with no groups.
    pub fn single(pool: LinkPool) -> Self {
        Self {
            default: Arc::new(pool),
            groups: Vec::new(),
        }
    }

    /// The pool a path goes to: the most specific group path, else the
    /// default group.
    pub fn select(&self, path: &str) -> Arc<LinkPool> {
        self.groups
            .iter()
            .find(|(prefix, _)| prefix_matches(prefix, path))
            .map(|(_, pool)| Arc::clone(pool))
            .unwrap_or_else(|| Arc::clone(&self.default))
    }

    /// The default group's pool.
    pub fn default_pool(&self) -> Arc<LinkPool> {
        Arc::clone(&self.default)
    }
}

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
    /// To this pool, with the name the application sees as SERVER_NAME.
    Serve {
        /// The application's workers.
        pool: Arc<LinkPool>,
        /// The matched or canonical host name, when a host matched.
        server_name: Option<String>,
    },
    /// Redirected to HTTPS: the host insists on it.
    Redirect,
    /// No virtual host answers to that name on this listener (421).
    Misdirected,
    /// The host has no route for the path (404).
    NoRoute,
}

impl Routing {
    /// One application and nothing else: development mode, or a daemon
    /// with no host declared.
    pub fn single(pools: AppPools) -> Self {
        Self {
            version: 0,
            trust: Trust::default(),
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
        // The default workers are one application's, found by its sockets
        // so its groups come along; without one, a plain pool.

        let default_app = snapshot
            .apps
            .iter()
            .find(|app| app.sockets == snapshot.worker_sockets);

        let default = match default_app {
            Some(app) => AppPools::connect(&paths(&app.sockets), &groups_of(app)).await?,
            None => AppPools::single(LinkPool::connect(&paths(&snapshot.worker_sockets)).await?),
        };

        let mut apps = HashMap::new();

        for app in &snapshot.apps {
            let pools = AppPools::connect(&paths(&app.sockets), &groups_of(app)).await?;
            apps.insert(app.app.clone(), Arc::new(pools));
        }

        // Exact names go in a map; wildcards keep their suffix, longest
        // first, so *.shop.example.com beats *.example.com.

        let trust = match &snapshot.proxy {
            Some(proxy) => Trust::from_snapshot(proxy).map_err(|error| anyhow::anyhow!(error))?,
            None => Trust::default(),
        };

        let mut routing = Self {
            version: snapshot.version,
            trust,
            default: Arc::new(default),
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
                routing.routes[index].push(Route {
                    path: route.path.clone(),
                    app: route.app.clone(),
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

    /// Whether any virtual host is declared.
    pub fn has_hosts(&self) -> bool {
        !self.hosts.is_empty()
    }

    /// The default application's default pool, which is what the
    /// dashboard counts.
    pub fn default_pool(&self) -> Arc<LinkPool> {
        self.default.default_pool()
    }

    /// Where a request goes, from the host the client asked for, the
    /// path, the listener it arrived on and whether that was over TLS.
    pub fn select(&self, name: Option<&str>, path: &str, listener: &str, tls: bool) -> Decision {
        if self.hosts.is_empty() {
            return Decision::Serve {
                pool: self.default.select(path),
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

        let pool = self
            .apps
            .get(&route.app)
            .map(|pools| pools.select(path))
            .unwrap_or_else(|| Arc::new(LinkPool::empty()));

        // SERVER_NAME is the name the request matched with, or the
        // canonical name when the host says so (section 4.3).

        let server_name = if host.server_name.is_empty() {
            name.map(normalise)
        } else {
            Some(host.server_name.clone())
        };

        Decision::Serve { pool, server_name }
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
                },
                pb::Route {
                    host: "shop".to_owned(),
                    path: "/api".to_owned(),
                    app: "api".to_owned(),
                },
                pb::Route {
                    host: "any".to_owned(),
                    path: "/".to_owned(),
                    app: "shop".to_owned(),
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
            Decision::Serve { pool, .. } => assert!(pool.is_empty(), "the api app has no workers"),
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
