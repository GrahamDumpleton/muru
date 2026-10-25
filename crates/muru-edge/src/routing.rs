//! Virtual hosts and routes (design specification sections 4.3 to 4.5):
//! the tables a snapshot compiles into, consulted once per request.
//!
//! Without any host the edge serves one application, as in development
//! mode: every request goes to its workers. With hosts, a request's host
//! name selects the virtual host (exact name, then the wildcard with the
//! longest suffix, then the listener's default host, else 421), the
//! longest route prefix selects the application, and the route's traffic
//! policy, when it has one, selects the release (section 9.4: a preview
//! header or cookie, then a sticky cookie, then the bucket the client's
//! key hashes to), else the application's serving release takes the
//! request. Within a release, the most specific worker group path selects
//! the group's workers, else the default group's (section 7.1).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use http::HeaderMap;
use muru_proto::edge::v1 as pb;

use crate::errors::{Chain, ErrorPolicy};
use crate::link::LinkPool;
use crate::proxy::{Network, Trust};
use crate::upstream::Upstream;

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
    /// The host's error pages and override (section 4.12).
    pub errors: Arc<ErrorPolicy>,
}

/// Maintenance mode for an application (section 4.12): the edge answers
/// 503 maintenance for it, except for what the exemptions let through.
#[derive(Debug)]
pub struct Maintenance {
    exempt_paths: Vec<String>,
    exempt_addresses: Vec<Network>,
    bypass_cookie: Option<(String, String)>,
    /// The Retry-After the page carries, in seconds.
    pub retry_after: u32,
}

impl Maintenance {
    /// Compile the snapshot's message; none when maintenance is off.
    pub fn from_message(message: &pb::Maintenance) -> Result<Option<Self>, String> {
        if !message.on {
            return Ok(None);
        }

        let exempt_addresses = message
            .exempt_addresses
            .iter()
            .map(|text| crate::proxy::parse_network(text))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("maintenance: exempt_addresses: {error}"))?;
        let bypass_cookie = message
            .bypass_cookie
            .split_once('=')
            .map(|(name, value)| (name.to_owned(), value.to_owned()));
        Ok(Some(Self {
            exempt_paths: message.exempt_paths.clone(),
            exempt_addresses,
            bypass_cookie,
            retry_after: u32::try_from(message.retry_after_s)
                .ok()
                .filter(|seconds| *seconds > 0)
                .unwrap_or(60),
        }))
    }

    /// Whether a request is served as usual: its path is exempt, or the
    /// client's address is, or it carries the bypass cookie.
    pub fn exempts(&self, path: &str, peer: IpAddr, headers: &HeaderMap) -> bool {
        self.exempt_paths
            .iter()
            .any(|prefix| prefix_matches(prefix, path))
            || self
                .exempt_addresses
                .iter()
                .any(|network| network.contains(peer))
            || self
                .bypass_cookie
                .as_ref()
                .is_some_and(|(name, value)| cookie(headers, name).as_deref() == Some(value))
    }
}

struct Route {
    path: String,
    app: String,
    /// The upstream that takes the requests (section 4.13), instead of
    /// an application.
    upstream: Option<Arc<Upstream>>,
    /// The directory served, for a static route (section 4.9).
    static_route: Option<Arc<crate::static_files::StaticRoute>>,
    /// The application that takes what no file answers.
    fallback_app: String,
    /// The upstream that takes what no file answers, when one does.
    fallback_upstream: Option<Arc<Upstream>>,
    /// How the application's releases share the requests (section 9.4).
    traffic: Option<Policy>,
    /// Reachable only through internal redirects (section 4.10).
    internal: bool,
    /// Applications other than the route's own whose redirects may
    /// reach it.
    accept_redirects_from: Vec<String>,
    /// The route's error pages and override (section 4.12).
    errors: Arc<ErrorPolicy>,
    /// The decision asked for before the route is served (section 4.16).
    auth: Option<Arc<crate::auth::Auth>>,
}

impl Route {
    /// The application the route belongs to: its own, or a static
    /// route's fallback; empty for a static route with none.
    fn owner(&self) -> &str {
        if self.app.is_empty() {
            &self.fallback_app
        } else {
            &self.app
        }
    }

    /// Whether a redirect issued by `app` may reach the route (section
    /// 4.10): its own application's always may, another's when listed.
    fn accepts(&self, app: &str) -> bool {
        self.owner() == app || self.accept_redirects_from.iter().any(|name| name == app)
    }
}

/// The application and release that issued an internal redirect (section
/// 4.10): what the sub-request's routing is judged against, and the
/// release a redirect back into the same application is pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// The application that redirected; empty for an error page
    /// sub-request (section 4.12) for a request no application took.
    pub app: String,
    /// The release that redirected.
    pub release: String,
    /// An error page sub-request (section 4.12): internal routes and
    /// mounts open to it without being asked, since the operator named
    /// the path.
    pub privileged: bool,
}

/// Which response forms a mount treats as internal redirects (section
/// 4.10), beyond X-Muru-Redirect, which always is one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RedirectFlags {
    /// X-Accel-Redirect is honoured.
    pub accel_redirect: bool,
    /// A 200 with a Location starting with `/` and no body is a redirect.
    pub cgi_local_redirect: bool,
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
    /// Reachable only through internal redirects (section 4.10).
    pub internal: bool,
    /// Other applications whose redirects may reach the mount when it is
    /// internal; the release's own always may.
    pub accept_redirects_from: Vec<String>,
    /// X-Accel-Redirect from the mount is an internal redirect.
    pub accel_redirect: bool,
    /// A 200 with a local Location and no body is an internal redirect.
    pub cgi_local_redirect: bool,
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
    /// Which response forms from the mount are internal redirects.
    pub redirects: RedirectFlags,
    /// Maintenance mode, when the application is in it (section 4.12).
    pub maintenance: Option<Arc<Maintenance>>,
    /// The upstream that takes the request instead of workers (section
    /// 4.13); `pool` is then empty.
    pub upstream: Option<Arc<Upstream>>,
}

/// What a request brings to the release choice (section 9.4): its
/// headers, for the preview, sticky and id cookies and a header key, and
/// the client's address.
pub struct Client<'a> {
    /// The request headers.
    pub headers: &'a HeaderMap,
    /// The client's address, the key of last resort.
    pub peer: IpAddr,
    /// Whether the response travels inside TLS, which marks the cookies
    /// the edge issues Secure.
    pub secure: bool,
    /// Issues an anonymous id to a client that has none yet.
    pub ids: &'a crate::errors::RequestIds,
}

/// An application's releases that may take requests: the serving one,
/// which takes what no policy assigns, and every release by id.
struct AppReleases {
    serving: Arc<AppPools>,
    releases: HashMap<String, Arc<AppPools>>,
}

/// The number of buckets a route's clients are spread over.
pub const BUCKETS: u32 = 10_000;

/// The anonymous id cookie the edge issues (section 9.4).
pub const ID_COOKIE: &str = "muru_id";

/// The cookie that keeps a client on the release it first got.
pub const STICKY_COOKIE: &str = "muru_release";

/// The cookie that pins a release for previews.
pub const PREVIEW_COOKIE: &str = "muru_preview";

/// The header that pins a release for previews. It is not an X-Muru
/// header, which the edge removes from every request (section 4.6).
pub const PREVIEW_HEADER: &str = "muru-preview";

/// A route's compiled traffic policy (section 9.4).
#[derive(Debug, Clone)]
pub struct Policy {
    /// Each share's target and bucket range, low inclusive, high
    /// exclusive, from the top down.
    shares: Vec<(Target, u32, u32)>,
    key: Key,
    salt: String,
    sticky: bool,
}

/// What a request is bucketed by.
#[derive(Debug, Clone)]
enum Key {
    /// The anonymous id cookie, issued when missing.
    Anonymous,
    /// A request header, lower-cased; the client address when missing.
    Header(String),
    /// A cookie; the client address when missing.
    Cookie(String),
    /// The client address.
    Client,
}

impl Policy {
    /// Compile a snapshot's policy, laying the shares out from the top
    /// bucket down in the order given.
    pub fn from_message(message: &pb::Traffic) -> Result<Self, String> {
        let key = match message.hash_key.as_str() {
            "" | "cookie" => Key::Anonymous,
            "client" => Key::Client,
            other => match other.split_once(':') {
                Some(("header", name)) if !name.is_empty() => {
                    Key::Header(name.to_ascii_lowercase())
                }
                Some(("cookie", name)) if !name.is_empty() => Key::Cookie(name.to_owned()),
                _ => {
                    return Err(format!(
                        "hash_key {other:?} is not cookie, header:NAME, cookie:NAME or client"
                    ));
                }
            },
        };

        let mut top = BUCKETS;
        let mut shares = Vec::new();

        for share in &message.shares {
            let low = top
                .checked_sub(share.weight)
                .ok_or_else(|| "the shares add up to more than the whole".to_owned())?;

            let target = if share.upstream.is_empty() {
                Target::Release(share.release.clone())
            } else {
                Target::Upstream(share.upstream.clone())
            };
            shares.push((target, low, top));
            top = low;
        }

        Ok(Self {
            shares,
            key,
            salt: message.salt.clone(),
            sticky: message.sticky,
        })
    }

    /// The bucket a key falls in: FNV-1a over the salt and the key, so
    /// the same client lands in the same bucket in every edge process
    /// and across upgrades.
    pub fn bucket(salt: &str, key: &str) -> u32 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;

        for byte in salt.bytes().chain(std::iter::once(0)).chain(key.bytes()) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }

        (hash % u64::from(BUCKETS)) as u32
    }

    /// The target whose share a bucket falls in, if any.
    fn share_for(&self, bucket: u32) -> Option<&Target> {
        self.shares
            .iter()
            .find(|(_, low, high)| (*low..*high).contains(&bucket))
            .map(|(target, _, _)| target)
    }
}

/// What takes a share of a route's requests: a release of the route's
/// application, or an upstream (section 4.13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A release, by id.
    Release(String),
    /// An upstream, by name.
    Upstream(String),
}

impl Target {
    /// The release id or the upstream's name.
    pub fn name(&self) -> &str {
        match self {
            Self::Release(name) | Self::Upstream(name) => name,
        }
    }

    /// The target as a preview or sticky cookie names it: the release
    /// id, or `upstream:NAME`.
    fn label(&self) -> String {
        match self {
            Self::Release(name) => name.clone(),
            Self::Upstream(name) => format!("{UPSTREAM_PREFIX}{name}"),
        }
    }
}

/// How a preview or sticky cookie names an upstream rather than a
/// release.
pub const UPSTREAM_PREFIX: &str = "upstream:";

/// What the release choice picked.
enum Chosen<'a> {
    Pools(&'a Arc<AppPools>),
    Upstream(Arc<Upstream>),
}

impl Chosen<'_> {
    /// The target by its cookie label.
    fn named<'a>(
        name: &str,
        app: &'a AppReleases,
        upstreams: &HashMap<String, Arc<Upstream>>,
    ) -> Option<Chosen<'a>> {
        match name.strip_prefix(UPSTREAM_PREFIX) {
            Some(upstream) => upstreams.get(upstream).cloned().map(Chosen::Upstream),
            None => app.releases.get(name).map(Chosen::Pools),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Pools(pools) => pools.labels().1.to_owned(),
            Self::Upstream(upstream) => format!("{UPSTREAM_PREFIX}{}", upstream.name()),
        }
    }

    fn select(self, path: &str, origin: Option<&Origin>) -> Option<Selection> {
        match self {
            Self::Pools(pools) => pools.select(path, origin),
            Self::Upstream(upstream) => Some(upstream_selection(&upstream)),
        }
    }
}

/// Where a request to an upstream goes: no workers, the upstream's name
/// as the application's for the access log, and `upstream` as its group.
fn upstream_selection(upstream: &Arc<Upstream>) -> Selection {
    Selection {
        pool: Arc::new(LinkPool::empty()),
        app: upstream.name().to_owned(),
        release: String::new(),
        group: crate::upstream::GROUP.to_owned(),
        mount: String::new(),
        script_name: String::new(),
        redirects: RedirectFlags::default(),
        maintenance: None,
        upstream: Some(Arc::clone(upstream)),
    }
}

/// Section 4.5: which of an application's releases takes a request. The
/// release a preview header or cookie names comes first, then the one a
/// sticky cookie records, then the policy's bucket for the client's key,
/// else the serving release. Cookies the edge issues on the way go into
/// `cookies` as Set-Cookie values.
fn choose<'a>(
    app: &'a AppReleases,
    policy: Option<&Policy>,
    client: &Client<'_>,
    cookies: &mut Vec<String>,
    upstreams: &HashMap<String, Arc<Upstream>>,
) -> Chosen<'a> {
    let preview = client
        .headers
        .get(PREVIEW_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| cookie(client.headers, PREVIEW_COOKIE));

    if let Some(chosen) = preview
        .as_deref()
        .and_then(|name| Chosen::named(name, app, upstreams))
    {
        return chosen;
    }

    let Some(policy) = policy else {
        return Chosen::Pools(&app.serving);
    };

    if policy.sticky
        && let Some(chosen) = cookie(client.headers, STICKY_COOKIE)
            .and_then(|name| Chosen::named(&name, app, upstreams))
    {
        return chosen;
    }

    let secure = if client.secure { "; Secure" } else { "" };

    let key = match &policy.key {
        Key::Anonymous => match cookie(client.headers, ID_COOKIE) {
            Some(id) => id,
            None => {
                let id = client.ids.next();
                cookies.push(format!(
                    "{ID_COOKIE}={id}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Lax{secure}"
                ));
                id
            }
        },
        Key::Header(name) => client
            .headers
            .get(name.as_str())
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .unwrap_or_else(|| client.peer.to_string()),
        Key::Cookie(name) => {
            cookie(client.headers, name).unwrap_or_else(|| client.peer.to_string())
        }
        Key::Client => client.peer.to_string(),
    };

    let bucket = Policy::bucket(&policy.salt, &key);
    let chosen = policy
        .share_for(bucket)
        .and_then(|target| Chosen::named(&target.label(), app, upstreams))
        .unwrap_or(Chosen::Pools(&app.serving));

    if policy.sticky {
        cookies.push(format!(
            "{STICKY_COOKIE}={}; Path=/; HttpOnly; SameSite=Lax{secure}",
            chosen.label()
        ));
    }

    chosen
}

/// The value of a cookie among the request's Cookie headers.
pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.trim().to_owned())
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
    /// The default mount's redirect forms (section 4.10).
    redirects: RedirectFlags,
    /// The release's error pages and override (section 4.12).
    errors: Arc<ErrorPolicy>,
    /// Maintenance mode, when the application is in it.
    maintenance: Option<Arc<Maintenance>>,
    /// Section 9.9: the instances of a managed HTTP release, which take
    /// every request in place of workers.
    server: Option<Arc<Upstream>>,
}

/// The group name a managed HTTP release's requests are logged under
/// (section 9.9).
pub const SERVER_GROUP: &str = "http";

impl AppPools {
    /// Connect the default workers and each group's. `app` and `release`
    /// label the requests served in the access log; `redirects` says
    /// which response forms from the default mount are internal
    /// redirects.
    pub async fn connect(
        app: &str,
        release: &str,
        default: &[std::path::PathBuf],
        groups: &[GroupSockets],
        mounts: &[MountPath],
        redirects: RedirectFlags,
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
                internal: mount.internal,
                accept_redirects_from: mount.accept_redirects_from.clone(),
                accel_redirect: mount.accel_redirect,
                cgi_local_redirect: mount.cgi_local_redirect,
            })
            .collect();
        mounts.sort_by_key(|mount| std::cmp::Reverse(mount.path.len()));

        Ok(Self {
            app: app.to_owned(),
            release: release.to_owned(),
            default,
            groups: connected,
            mounts,
            redirects,
            errors: Arc::new(ErrorPolicy::default()),
            maintenance: None,
            server: None,
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
            redirects: RedirectFlags::default(),
            errors: Arc::new(ErrorPolicy::default()),
            maintenance: None,
            server: None,
        }
    }

    /// The same release served by the instances of a managed HTTP server
    /// (section 9.9) instead of workers.
    pub fn with_server(mut self, server: Arc<Upstream>) -> Self {
        self.server = Some(server);
        self
    }

    /// The instances of a managed HTTP release, when the release is one.
    pub fn server(&self) -> Option<&Arc<Upstream>> {
        self.server.as_ref()
    }

    /// The same pools with the release's error pages and override
    /// (section 4.12).
    pub fn with_errors(mut self, errors: Arc<ErrorPolicy>) -> Self {
        self.errors = errors;
        self
    }

    /// The same pools in maintenance mode, or out of it.
    pub fn with_maintenance(mut self, maintenance: Option<Arc<Maintenance>>) -> Self {
        self.maintenance = maintenance;
        self
    }

    /// Where a path goes: the most specific group path's pool, else the
    /// default group's, and the longest mount prefix, else the default
    /// mount (section 6.7). An internal mount (section 4.10) is reached
    /// only by a redirect from the release's own application, or from
    /// one it accepts: a client's request for it, or an unaccepted
    /// redirect, gets nothing.
    pub fn select(&self, path: &str, origin: Option<&Origin>) -> Option<Selection> {
        if let Some(server) = &self.server {
            return Some(Selection {
                pool: Arc::new(LinkPool::empty()),
                app: self.app.clone(),
                release: self.release.clone(),
                group: SERVER_GROUP.to_owned(),
                mount: String::new(),
                script_name: String::new(),
                redirects: RedirectFlags::default(),
                maintenance: self.maintenance.clone(),
                upstream: Some(Arc::clone(server)),
            });
        }

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

        if let Some(mount) = mount
            && mount.internal
        {
            let accepted = origin.is_some_and(|origin| {
                origin.app == self.app
                    || mount
                        .accept_redirects_from
                        .iter()
                        .any(|name| name == &origin.app)
            });

            if !accepted {
                return None;
            }
        }

        Some(Selection {
            pool,
            app: self.app.clone(),
            release: self.release.clone(),
            group,
            mount: mount.map(|mount| mount.name.clone()).unwrap_or_default(),
            script_name: mount.map(|mount| mount.path.clone()).unwrap_or_default(),
            redirects: mount.map_or(self.redirects, |mount| RedirectFlags {
                accel_redirect: mount.accel_redirect,
                cgi_local_redirect: mount.cgi_local_redirect,
            }),
            maintenance: self.maintenance.clone(),
            upstream: None,
        })
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
    apps: HashMap<String, AppReleases>,
    hosts: Vec<Host>,
    exact: HashMap<String, usize>,
    wildcards: Vec<(String, usize)>,
    default_host: Option<usize>,
    routes: Vec<Vec<Route>>,
    /// The node's error pages and override (section 4.12).
    errors: Arc<ErrorPolicy>,
    /// The HTTP upstreams by name (section 4.13).
    upstreams: HashMap<String, Arc<Upstream>>,
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
        /// Set-Cookie values the edge issues with the response: the
        /// anonymous id and the sticky release (section 9.4).
        cookies: Vec<String>,
        /// The check the route asks for first (section 4.16).
        auth: Option<Arc<crate::auth::Auth>>,
    },
    /// Redirected to HTTPS: the host insists on it.
    Redirect,
    /// No virtual host answers to that name on this listener (421).
    Misdirected,
    /// The host has no route for the path (404).
    NoRoute,
    /// The route or mount is internal (section 4.10) and the request
    /// did not arrive by a redirect that may reach it (404).
    InternalOnly,
    /// A redirect from another application that the route does not
    /// accept (section 4.10), treated as no route; `app` is the one that
    /// redirected, for the log.
    NotAccepted {
        /// The application that redirected.
        app: String,
    },
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
        /// Set-Cookie values the edge issues with the response.
        cookies: Vec<String>,
        /// The check the route asks for first (section 4.16).
        auth: Option<Arc<crate::auth::Auth>>,
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
            errors: Arc::new(ErrorPolicy::default()),
            upstreams: HashMap::new(),
        }
    }

    /// Compile a snapshot, connecting to every worker it names first, so
    /// a snapshot that cannot be reached is refused whole.
    pub async fn from_snapshot(snapshot: &pb::Snapshot) -> anyhow::Result<Self> {
        let mut apps: HashMap<String, AppReleases> = HashMap::new();
        let mut upstreams: HashMap<String, Arc<Upstream>> = HashMap::new();

        for message in &snapshot.upstreams {
            let upstream = Upstream::from_message(message)
                .map_err(|error| anyhow::anyhow!("upstream {}: {error}", message.name))?;
            upstreams.insert(message.name.clone(), Arc::new(upstream));
        }

        let upstream_named =
            |name: &str, route: &pb::Route| -> anyhow::Result<Option<Arc<Upstream>>> {
                if name.is_empty() {
                    return Ok(None);
                }

                upstreams.get(name).cloned().map(Some).ok_or_else(|| {
                    anyhow::anyhow!(
                        "route {} {}: no upstream named {name}",
                        route.host,
                        route.path
                    )
                })
            };

        for app in &snapshot.apps {
            let errors = ErrorPolicy::load(&app.error_pages, app.error_override.as_ref())
                .await
                .map_err(|error| anyhow::anyhow!("{}@{}: {error}", app.app, app.release))?;
            let maintenance = app
                .maintenance
                .as_ref()
                .map(Maintenance::from_message)
                .transpose()
                .map_err(|error| anyhow::anyhow!("{}@{}: {error}", app.app, app.release))?
                .flatten()
                .map(Arc::new);
            let mut pools = AppPools::connect(
                &app.app,
                &app.release,
                &paths(&app.sockets),
                &groups_of(app),
                &mounts_of(app),
                RedirectFlags {
                    accel_redirect: app.accel_redirect,
                    cgi_local_redirect: app.cgi_local_redirect,
                },
            )
            .await?
            .with_errors(Arc::new(errors))
            .with_maintenance(maintenance);

            // Section 9.9: a managed HTTP release's instances stand in
            // for its workers, as an upstream named after the release.

            if let Some(server) = &app.server {
                let server = Upstream::from_message(server)
                    .map_err(|error| anyhow::anyhow!("{}@{}: {error}", app.app, app.release))?;
                pools = pools.with_server(Arc::new(server));
            }

            let pools = Arc::new(pools);
            let releases = apps.entry(app.app.clone()).or_insert_with(|| AppReleases {
                serving: Arc::clone(&pools),
                releases: HashMap::new(),
            });

            if app.serving {
                releases.serving = Arc::clone(&pools);
            }

            releases.releases.insert(app.release.clone(), pools);
        }

        // The default workers are one application's, found by its sockets
        // so its groups come along and its pools are shared; without one,
        // a plain pool.

        let default_app = if snapshot.default_app.is_empty() {
            snapshot
                .apps
                .iter()
                .find(|app| app.sockets == snapshot.worker_sockets)
                .and_then(|app| apps.get(&app.app)?.releases.get(&app.release))
        } else {
            apps.get(&snapshot.default_app)
                .and_then(|releases| releases.releases.get(&snapshot.default_release))
        };

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

        let errors = ErrorPolicy::load(&snapshot.error_pages, snapshot.error_override.as_ref())
            .await
            .map_err(|error| anyhow::anyhow!(error))?;

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
            errors: Arc::new(errors),
            upstreams: upstreams.clone(),
        };

        for host in &snapshot.hosts {
            let index = routing.hosts.len();
            let errors = ErrorPolicy::load(&host.error_pages, host.error_override.as_ref())
                .await
                .map_err(|error| anyhow::anyhow!("host {}: {error}", host.name))?;

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
                errors: Arc::new(errors),
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
                let traffic = route
                    .traffic
                    .as_ref()
                    .map(Policy::from_message)
                    .transpose()
                    .map_err(|error| {
                        anyhow::anyhow!("route {} {}: traffic: {error}", route.host, route.path)
                    })?;
                let errors = ErrorPolicy::load(&route.error_pages, route.error_override.as_ref())
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("route {} {}: {error}", route.host, route.path)
                    })?;
                let auth = route
                    .auth
                    .as_ref()
                    .map(crate::auth::Auth::from_message)
                    .transpose()
                    .map_err(|error| {
                        anyhow::anyhow!("route {} {}: {error}", route.host, route.path)
                    })?
                    .map(Arc::new);
                routing.routes[index].push(Route {
                    path: route.path.clone(),
                    app: route.app.clone(),
                    upstream: upstream_named(&route.upstream, route)?,
                    static_route,
                    fallback_app: route.fallback_app.clone(),
                    fallback_upstream: upstream_named(&route.fallback_upstream, route)?,
                    traffic,
                    internal: route.internal,
                    accept_redirects_from: route.accept_redirects_from.clone(),
                    errors: Arc::new(errors),
                    auth,
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

    /// The upstreams, for their health gauges.
    pub fn upstreams(&self) -> Vec<Arc<Upstream>> {
        self.upstreams.values().cloned().collect()
    }

    /// The instances of every managed HTTP release (section 9.9), each
    /// as the upstream that stands in for the release's workers.
    pub fn servers(&self) -> Vec<Arc<Upstream>> {
        let mut servers: Vec<Arc<Upstream>> = self
            .app_pools()
            .iter()
            .filter_map(|pools| pools.server().cloned())
            .collect();
        servers.sort_by(|left, right| left.name().cmp(right.name()));
        servers
    }

    /// The error pages and override in force for a request (section
    /// 4.12), most specific first: the route's, the release's (the
    /// application the request was routed to, as `located` names it),
    /// the host's and the node's.
    pub fn errors_for(
        &self,
        name: Option<&str>,
        path: &str,
        listener: &str,
        located: Option<(&str, &str)>,
    ) -> Chain {
        let mut levels = Vec::with_capacity(4);

        if self.hosts.is_empty() {
            levels.push(Arc::clone(&self.default.errors));
            levels.push(Arc::clone(&self.errors));
            return Chain::new(levels);
        }

        let host = self
            .host_for(name, listener)
            .or_else(|| name.and_then(|name| self.hosts.iter().position(|host| host.name == name)));

        if let Some(index) = host
            && let Some(route) = self.routes[index]
                .iter()
                .find(|route| prefix_matches(&route.path, path))
        {
            levels.push(Arc::clone(&route.errors));
        }

        if let Some((app, release)) = located
            && let Some(releases) = self.apps.get(app)
            && let Some(pools) = releases.releases.get(release)
        {
            levels.push(Arc::clone(&pools.errors));
        }

        if let Some(index) = host {
            levels.push(Arc::clone(&self.hosts[index].errors));
        }

        levels.push(Arc::clone(&self.errors));
        Chain::new(levels)
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
        let mut pools: Vec<Arc<AppPools>> = self
            .apps
            .values()
            .flat_map(|releases| releases.releases.values().cloned())
            .collect();
        pools.sort_by(|left, right| left.labels().cmp(&right.labels()));

        if !pools.iter().any(|seen| Arc::ptr_eq(seen, &self.default)) {
            pools.insert(0, Arc::clone(&self.default));
        }

        pools
    }

    /// Where a request goes, from the host the client asked for, the
    /// path, the listener it arrived on, whether that was over TLS, and
    /// what the request brings to the release choice. With an `origin`
    /// the request is an internal redirect (section 4.10): internal
    /// routes and mounts open to it, a route of another application only
    /// when it accepts the origin's, and a route back into the origin's
    /// application is pinned to its release.
    pub fn select(
        &self,
        name: Option<&str>,
        path: &str,
        listener: &str,
        tls: bool,
        client: &Client<'_>,
        origin: Option<&Origin>,
    ) -> Decision {
        if self.hosts.is_empty() {
            return match self.default.select(path, origin) {
                Some(selection) => Decision::Serve {
                    selection,
                    server_name: None,
                    cookies: Vec::new(),
                    auth: None,
                },
                None => Decision::InternalOnly,
            };
        }

        // A redirect may name a host by its configured name as well as
        // by one of the names clients use.

        let Some(index) = self.host_for(name, listener).or_else(|| {
            origin.and_then(|_| {
                name.and_then(|name| self.hosts.iter().position(|host| host.name == name))
            })
        }) else {
            return Decision::Misdirected;
        };
        let host = &self.hosts[index];

        if host.https_redirect && !tls && origin.is_none() {
            return Decision::Redirect;
        }

        let Some(route) = self.routes[index]
            .iter()
            .find(|route| prefix_matches(&route.path, path))
        else {
            return Decision::NoRoute;
        };

        match origin {
            None if route.internal => return Decision::InternalOnly,
            Some(origin) if !origin.privileged && !route.accepts(&origin.app) => {
                return Decision::NotAccepted {
                    app: origin.app.clone(),
                };
            }
            _ => {}
        }

        let mut cookies = Vec::new();

        if let Some(static_route) = &route.static_route {
            let fallback = if let Some(upstream) = &route.fallback_upstream {
                Some(upstream_selection(upstream))
            } else if route.fallback_app.is_empty() {
                None
            } else {
                self.selection_for(
                    &route.fallback_app,
                    route.traffic.as_ref(),
                    path,
                    client,
                    &mut cookies,
                    origin,
                )
            };
            return Decision::Static {
                route: Arc::clone(static_route),
                prefix: route.path.clone(),
                fallback,
                server_name: Some(if host.server_name.is_empty() {
                    name.map(normalise).unwrap_or_default()
                } else {
                    host.server_name.clone()
                }),
                cookies,
                auth: route.auth.clone(),
            };
        }

        let selection = match &route.upstream {
            Some(upstream) => upstream_selection(upstream),
            None => {
                let Some(selection) = self.selection_for(
                    &route.app,
                    route.traffic.as_ref(),
                    path,
                    client,
                    &mut cookies,
                    origin,
                ) else {
                    return Decision::InternalOnly;
                };
                selection
            }
        };

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
            cookies,
            auth: route.auth.clone(),
        }
    }

    /// The workers of an application for a sub-request the edge makes
    /// for itself (section 4.16): the application's serving release, or
    /// the one a preview or sticky cookie picks, and within it the mount
    /// for the path, internal ones included, since the operator named
    /// the path. None when the application has no mount for it.
    pub fn select_app(&self, app: &str, path: &str, client: &Client<'_>) -> Option<Selection> {
        let origin = Origin {
            app: String::new(),
            release: String::new(),
            privileged: true,
        };

        if self.hosts.is_empty() {
            return self.default.select(path, Some(&origin));
        }

        let mut cookies = Vec::new();
        self.selection_for(app, None, path, client, &mut cookies, Some(&origin))
    }

    /// An upstream for a sub-request the edge makes for itself (section
    /// 4.16), by name.
    pub fn select_upstream(&self, name: &str) -> Option<Selection> {
        self.upstreams.get(name).map(upstream_selection)
    }

    /// The workers of an application for a request: the release the
    /// route's policy and the request choose, or the origin's own release
    /// for a redirect back into its application (section 4.10), and
    /// within it the group and mount for the path. An application the
    /// snapshot does not name has no workers, and the request fails as
    /// unavailable. None when the path's mount is internal and the
    /// request may not reach it.
    fn selection_for(
        &self,
        app: &str,
        policy: Option<&Policy>,
        path: &str,
        client: &Client<'_>,
        cookies: &mut Vec<String>,
        origin: Option<&Origin>,
    ) -> Option<Selection> {
        match self.apps.get(app) {
            Some(releases) => {
                let pinned = origin
                    .filter(|origin| origin.app == app)
                    .and_then(|origin| releases.releases.get(&origin.release));

                match pinned {
                    Some(pools) => pools.select(path, origin),
                    None => choose(releases, policy, client, cookies, &self.upstreams)
                        .select(path, origin),
                }
            }
            None => Some(Selection {
                pool: Arc::new(LinkPool::empty()),
                app: app.to_owned(),
                release: String::new(),
                group: DEFAULT_GROUP.to_owned(),
                mount: String::new(),
                script_name: String::new(),
                redirects: RedirectFlags::default(),
                maintenance: None,
                upstream: None,
            }),
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
            internal: mount.internal,
            accept_redirects_from: mount.accept_redirects_from.clone(),
            accel_redirect: mount.accel_redirect,
            cgi_local_redirect: mount.cgi_local_redirect,
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
            default_app: String::new(),
            default_release: String::new(),
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
            error_pages: Vec::new(),
            error_override: None,
            upstreams: Vec::new(),
        }
    }

    fn routing() -> Routing {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(Routing::from_snapshot(&snapshot()))
            .unwrap()
    }

    /// A request with these headers, from a fixed client.
    fn client<'a>(headers: &'a HeaderMap, ids: &'a crate::errors::RequestIds) -> Client<'a> {
        Client {
            headers,
            peer: "10.0.0.1".parse().unwrap(),
            secure: false,
            ids,
        }
    }

    /// Select with no headers of note.
    fn select(
        routing: &Routing,
        name: Option<&str>,
        path: &str,
        listener: &str,
        tls: bool,
    ) -> Decision {
        let headers = HeaderMap::new();
        let ids = crate::errors::RequestIds::new();
        routing.select(name, path, listener, tls, &client(&headers, &ids), None)
    }

    fn serves(decision: Decision) -> Option<String> {
        match decision {
            Decision::Serve { server_name, .. } => server_name,
            Decision::Redirect => Some("redirect".to_owned()),
            Decision::Misdirected => Some("421".to_owned()),
            Decision::NoRoute => Some("404".to_owned()),
            Decision::InternalOnly => Some("internal".to_owned()),
            Decision::NotAccepted { .. } => Some("not accepted".to_owned()),
            Decision::Static { server_name, .. } => server_name,
        }
    }

    #[test]
    fn names_select_hosts_in_the_order_of_section_4_3() {
        let routing = routing();
        let plain = "127.0.0.1:8080";

        assert_eq!(
            serves(select(
                &routing,
                Some("shop.example.com:8443"),
                "/",
                plain,
                true
            )),
            Some("shop.example.com".to_owned()),
            "exact, case and port aside, with the canonical name"
        );
        assert_eq!(
            serves(select(
                &routing,
                Some("eu.shop.example.com"),
                "/",
                plain,
                true
            )),
            Some("shop.example.com".to_owned()),
            "the longest wildcard suffix wins"
        );
        assert_eq!(
            serves(select(
                &routing,
                Some("Blog.example.com"),
                "/",
                plain,
                false
            )),
            Some("blog.example.com".to_owned()),
            "a wildcard match gives the name asked for"
        );
        assert_eq!(
            serves(select(&routing, Some("a.b.example.com"), "/", plain, false)),
            Some("a.b.example.com".to_owned()),
            "two labels do not match a single-label wildcard, so the default host answers"
        );
        assert_eq!(
            serves(select(
                &routing,
                Some("a.b.example.com"),
                "/",
                "127.0.0.1:8443",
                true
            )),
            Some("421".to_owned()),
            "the default host answers only on its listener"
        );
        assert_eq!(
            serves(select(&routing, None, "/", plain, false)),
            None,
            "no host header falls to the default host"
        );
    }

    #[test]
    fn redirects_and_routes() {
        let routing = routing();
        let plain = "127.0.0.1:8080";

        assert_eq!(
            serves(select(
                &routing,
                Some("shop.example.com"),
                "/",
                plain,
                false
            )),
            Some("redirect".to_owned())
        );

        match select(
            &routing,
            Some("shop.example.com"),
            "/api/users",
            plain,
            true,
        ) {
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

    /// A snapshot whose shop application has two releases, r1 serving
    /// and r2 with a share of the root route, sticky and keyed by the
    /// anonymous cookie.
    fn split_snapshot(sticky: bool, hash_key: &str) -> pb::Snapshot {
        let mut snapshot = snapshot();
        snapshot.apps = vec![
            pb::AppWorkers {
                app: "shop".to_owned(),
                release: "r1".to_owned(),
                serving: true,
                ..Default::default()
            },
            pb::AppWorkers {
                app: "shop".to_owned(),
                release: "r2".to_owned(),
                ..Default::default()
            },
        ];
        snapshot.routes[0].traffic = Some(pb::Traffic {
            shares: vec![pb::Share {
                release: "r2".to_owned(),
                weight: 2500,
                upstream: String::new(),
            }],
            hash_key: hash_key.to_owned(),
            salt: "shop /".to_owned(),
            sticky,
            sticky_grace_s: 600,
        });
        snapshot
    }

    fn release_of(decision: Decision) -> (String, Vec<String>) {
        match decision {
            Decision::Serve {
                selection, cookies, ..
            } => (selection.release, cookies),
            _ => panic!("not served"),
        }
    }

    #[test]
    fn buckets_are_laid_out_from_the_top_and_are_stable() {
        let policy = Policy::from_message(&pb::Traffic {
            shares: vec![
                pb::Share {
                    release: "r3".to_owned(),
                    weight: 1000,
                    upstream: String::new(),
                },
                pb::Share {
                    release: "r2".to_owned(),
                    weight: 2500,
                    upstream: String::new(),
                },
            ],
            ..Default::default()
        })
        .unwrap();
        let share_for = |bucket: u32| policy.share_for(bucket).map(Target::name);
        assert_eq!(share_for(9999), Some("r3"));
        assert_eq!(share_for(9000), Some("r3"));
        assert_eq!(share_for(8999), Some("r2"));
        assert_eq!(share_for(6500), Some("r2"));
        assert_eq!(share_for(6499), None, "the rest is the serving release's");
        assert_eq!(
            Policy::bucket("shop /", "user-42"),
            Policy::bucket("shop /", "user-42"),
            "the same key lands in the same bucket"
        );
        assert_ne!(
            Policy::bucket("shop /", "user-42"),
            Policy::bucket("shop /api", "user-42"),
            "the salt moves it"
        );
        assert!(
            Policy::from_message(&pb::Traffic {
                shares: vec![pb::Share {
                    release: "r2".to_owned(),
                    weight: 10_001,
                    upstream: String::new(),
                }],
                ..Default::default()
            })
            .is_err(),
            "shares beyond the whole are refused"
        );
        assert!(
            Policy::from_message(&pb::Traffic {
                hash_key: "header:".to_owned(),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn the_policy_spreads_clients_and_issues_an_id_cookie() {
        let routing = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(Routing::from_snapshot(&split_snapshot(false, "")))
            .unwrap();
        let ids = crate::errors::RequestIds::new();
        let mut seen = std::collections::HashSet::new();

        for n in 0..200 {
            let mut headers = HeaderMap::new();
            headers.insert(
                http::header::COOKIE,
                format!("{ID_COOKIE}=client-{n}").parse().unwrap(),
            );
            let (release, cookies) = release_of(routing.select(
                Some("shop.example.com"),
                "/",
                "127.0.0.1:8080",
                true,
                &client(&headers, &ids),
                None,
            ));
            assert!(cookies.is_empty(), "a client with an id gets no cookie");
            seen.insert(release);
        }

        assert_eq!(seen.len(), 2, "both releases take clients: {seen:?}");

        let headers = HeaderMap::new();
        let (_, cookies) = release_of(routing.select(
            Some("shop.example.com"),
            "/",
            "127.0.0.1:8080",
            true,
            &client(&headers, &ids),
            None,
        ));
        assert_eq!(cookies.len(), 1);
        assert!(
            cookies[0].starts_with(&format!("{ID_COOKIE}=")) && cookies[0].contains("HttpOnly"),
            "{cookies:?}"
        );
    }

    #[test]
    fn preview_and_sticky_cookies_pin_the_release() {
        let routing = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(Routing::from_snapshot(&split_snapshot(
                true,
                "header:x-user",
            )))
            .unwrap();
        let ids = crate::errors::RequestIds::new();
        let plain = "127.0.0.1:8080";

        let mut headers = HeaderMap::new();
        headers.insert(PREVIEW_HEADER, "r2".parse().unwrap());
        let (release, cookies) = release_of(routing.select(
            Some("shop.example.com"),
            "/",
            plain,
            true,
            &client(&headers, &ids),
            None,
        ));
        assert_eq!(
            (release.as_str(), cookies.len()),
            ("r2", 0),
            "the preview header pins"
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("{PREVIEW_COOKIE}=r9").parse().unwrap(),
        );
        headers.insert(PREVIEW_HEADER, "r9".parse().unwrap());
        let (release, _) = release_of(routing.select(
            Some("shop.example.com"),
            "/",
            plain,
            true,
            &client(&headers, &ids),
            None,
        ));
        assert_ne!(release, "r9", "an unknown release is ignored");

        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("a=b; {STICKY_COOKIE}=r2; c=d").parse().unwrap(),
        );
        let (release, cookies) = release_of(routing.select(
            Some("shop.example.com"),
            "/",
            plain,
            true,
            &client(&headers, &ids),
            None,
        ));
        assert_eq!(
            (release.as_str(), cookies.len()),
            ("r2", 0),
            "a sticky cookie holds"
        );

        // Keyed by a header: the same user gets the same release and a
        // sticky cookie naming it; no id cookie is issued.

        let mut headers = HeaderMap::new();
        headers.insert("x-user", "alice".parse().unwrap());
        let (first, cookies) = release_of(routing.select(
            Some("shop.example.com"),
            "/",
            plain,
            true,
            &client(&headers, &ids),
            None,
        ));
        assert_eq!(cookies.len(), 1, "{cookies:?}");
        assert!(
            cookies[0].starts_with(&format!("{STICKY_COOKIE}={first};")),
            "{cookies:?}"
        );
        let (again, _) = release_of(routing.select(
            Some("shop.example.com"),
            "/",
            plain,
            true,
            &client(&headers, &ids),
            None,
        ));
        assert_eq!(first, again);
    }

    #[test]
    fn without_hosts_everything_is_served() {
        let routing = Routing::single(AppPools::single(LinkPool::empty()));
        assert!(!routing.has_hosts());
        assert!(matches!(
            select(&routing, Some("whatever"), "/", "127.0.0.1:1", false),
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
