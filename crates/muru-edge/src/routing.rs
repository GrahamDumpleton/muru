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
use std::net::{IpAddr, SocketAddr};
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
    /// The rate limits every request for the host draws from (section
    /// 4.15).
    pub rate_limits: Vec<Arc<crate::ratelimit::Limit>>,
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
    /// The client classes the route takes (section 4.14); every request
    /// when empty.
    classes: Vec<String>,
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
    /// How uploads are received before dispatch (section 4.11).
    upload: Option<Arc<crate::uploads::Policy>>,
    /// The route's body limit, in place of the edge's (section 4.6).
    max_body_size: Option<u64>,
    /// The rate limits the route's requests draw from after the host's
    /// (section 4.15).
    rate_limits: Vec<Arc<crate::ratelimit::Limit>>,
    /// The methods the route refuses.
    methods: Option<Arc<crate::ratelimit::Methods>>,
    /// The site mode the application sees.
    mode: Option<Arc<str>>,
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

    /// Whether the route takes a request for `path` from a client of
    /// `class`.
    fn matches(&self, path: &str, class: &str) -> bool {
        prefix_matches(&self.path, path)
            && (self.classes.is_empty() || self.classes.iter().any(|name| name == class))
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

/// Section 4.9: the file hints by path a mount's, an upstream's or a
/// managed server's responses may give.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sendfile {
    /// Directories a hinted path must lie in, compared by canonical path
    /// when the hint arrives; with none every path hint is refused.
    pub roots: Vec<std::path::PathBuf>,
    /// A second response header honoured as X-Muru-Sendfile is, in lower
    /// case: `x-sendfile` for the frameworks that send that one.
    pub header: Option<String>,
}

impl Sendfile {
    /// The policy a snapshot's roots and header name describe.
    pub fn new(roots: &[String], header: &str) -> Arc<Self> {
        Arc::new(Self {
            roots: roots.iter().map(std::path::PathBuf::from).collect(),
            header: (!header.is_empty()).then(|| header.to_ascii_lowercase()),
        })
    }
}

/// The worker sockets of one group of a release, as a snapshot or a
/// development run names them.
#[derive(Debug, Clone)]
pub struct GroupSockets {
    /// The group's name.
    pub name: String,
    /// The paths it serves: prefixes, with an optional trailing `*`.
    pub paths: Vec<String>,
    /// The client classes it takes (section 4.14), ahead of the groups
    /// that name none; every path of theirs without `paths`.
    pub client_classes: Vec<String>,
    /// The hosts it takes, lower case; every host when empty (ADR 0033).
    pub hosts: Vec<String>,
    /// The methods it takes, upper case; every method when empty (ADR
    /// 0033).
    pub methods: Vec<String>,
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
    /// The file hints by path the mount may give (section 4.9).
    pub sendfile: Arc<Sendfile>,
}

/// A directory of a release's own files (section 4.9), served under a
/// path prefix ahead of the application, which takes what no file
/// answers.
#[derive(Debug, Clone)]
pub struct StaticPath {
    /// The prefix, without a trailing slash unless it is `/`.
    pub prefix: String,
    /// The directory and its settings.
    pub route: Arc<crate::static_files::StaticRoute>,
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
    /// The file hints by path the mount, upstream or server may give
    /// (section 4.9).
    pub sendfile: Arc<Sendfile>,
    /// Maintenance mode, when the application is in it (section 4.12).
    pub maintenance: Option<Arc<Maintenance>>,
    /// The upstream that takes the request instead of workers (section
    /// 4.13); `pool` is then empty.
    pub upstream: Option<Arc<Upstream>>,
    /// The release's own directory the path lies under (section 4.9),
    /// tried for a file before the request reaches the workers.
    pub static_path: Option<StaticPath>,
    /// The origins that may call the release through gRPC-Web (section
    /// 6.13).
    pub grpc_web: Arc<crate::grpc::WebOrigins>,
}

/// What a request brings to the release choice (section 9.4): its
/// headers, for the preview, sticky and id cookies and a header key, and
/// the client's address.
#[derive(Clone, Copy)]
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
    /// The client's class (section 4.14), which routes, policies and
    /// worker groups may be for.
    pub client_class: &'a str,
    /// The host name the request carries, which a worker group may be
    /// for (ADR 0033).
    pub host: Option<&'a str>,
    /// The configured name of the virtual host the request reached,
    /// which the routing fills in once it has chosen one; none before.
    pub virtual_host: Option<&'a str>,
    /// The request's method, which a worker group may be for.
    pub method: &'a str,
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
    /// The target that takes every request of a client class.
    classes: HashMap<String, Target>,
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

        let classes = message
            .classes
            .iter()
            .map(|(class, name)| {
                let target = match name.strip_prefix(UPSTREAM_PREFIX) {
                    Some(upstream) => Target::Upstream(upstream.to_owned()),
                    None => Target::Release(name.clone()),
                };

                (class.clone(), target)
            })
            .collect();

        Ok(Self {
            shares,
            key,
            salt: message.salt.clone(),
            sticky: message.sticky,
            classes,
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

    fn select(self, path: &str, client: &Client<'_>, origin: Option<&Origin>) -> Option<Selection> {
        match self {
            Self::Pools(pools) => pools.select(path, client, origin),
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
        sendfile: upstream.sendfile(),
        maintenance: None,
        upstream: Some(Arc::clone(upstream)),
        grpc_web: Arc::default(),
        static_path: None,
    }
}

/// Section 4.5: which of an application's releases takes a request. The
/// release a preview header or cookie names comes first, then the one the
/// policy gives the client's class (section 4.14), then the one a sticky
/// cookie records, then the policy's bucket for the client's key, else
/// the serving release. Cookies the edge issues on the way go into
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

    if let Some(chosen) = policy
        .classes
        .get(client.client_class)
        .and_then(|target| Chosen::named(&target.label(), app, upstreams))
    {
        return chosen;
    }

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
    /// The groups by path, most specific first, those for client classes
    /// ahead of the rest.
    groups: Vec<GroupPath>,
    mounts: Vec<MountPath>,
    /// The release's own directories of files (section 4.9), the longest
    /// prefix first.
    statics: Vec<StaticPath>,
    /// The default mount's redirect forms (section 4.10).
    redirects: RedirectFlags,
    /// The default mount's file hints by path (section 4.9).
    sendfile: Arc<Sendfile>,
    /// The release's error pages and override (section 4.12).
    errors: Arc<ErrorPolicy>,
    /// Maintenance mode, when the application is in it.
    maintenance: Option<Arc<Maintenance>>,
    /// Section 9.9: the instances of a managed HTTP release, which take
    /// every request in place of workers.
    server: Option<Arc<Upstream>>,
    /// Section 6.13: the origins that may call it through gRPC-Web.
    grpc_web: Arc<crate::grpc::WebOrigins>,
}

/// A path that selects a worker group (section 7.1).
struct GroupPath {
    /// The prefix.
    prefix: String,
    /// The client classes the group is for; every class when empty.
    classes: Vec<String>,
    /// The hosts the group is for; every host when empty.
    hosts: Vec<String>,
    /// The methods the group is for; every method when empty.
    methods: Vec<String>,
    /// The group's name.
    name: String,
    /// Its workers.
    pool: Arc<LinkPool>,
}

impl GroupPath {
    /// How many lists the group asks with, for its rank (ADR 0033).
    fn lists(&self) -> usize {
        [
            !self.classes.is_empty(),
            !self.hosts.is_empty(),
            !self.methods.is_empty(),
        ]
        .into_iter()
        .filter(|given| *given)
        .count()
    }

    /// Whether a request of the client's class, carrying `carried` as
    /// its host to the client's virtual host, with its method, satisfies
    /// every list the group gives.
    fn takes(&self, client: &Client<'_>, carried: Option<&str>) -> bool {
        (self.classes.is_empty() || self.classes.iter().any(|name| name == client.client_class))
            && (self.hosts.is_empty()
                || self.hosts.iter().any(|host| {
                    Some(host.as_str()) == carried || Some(host.as_str()) == client.virtual_host
                }))
            && (self.methods.is_empty()
                || self
                    .methods
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(client.method)))
    }
}

/// The group name a managed HTTP release's requests are logged under
/// (section 9.9).
pub const SERVER_GROUP: &str = "http";

impl AppPools {
    /// The workers of any of this release's pools that could not be
    /// connected to, each once.
    pub fn unreachable(&self) -> Vec<crate::link::Unreachable> {
        let mut pools: Vec<&Arc<LinkPool>> = vec![&self.default];

        for group in &self.groups {
            if !pools.iter().any(|pool| Arc::ptr_eq(pool, &group.pool)) {
                pools.push(&group.pool);
            }
        }

        pools
            .into_iter()
            .flat_map(|pool| pool.unreachable().iter().cloned())
            .collect()
    }

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

            // Section 4.14 and ADR 0033: a group with match lists and no
            // paths of its own takes the requests they name for every path.

            let root = ["/".to_owned()];
            let asks = !group.client_classes.is_empty()
                || !group.hosts.is_empty()
                || !group.methods.is_empty();

            let paths = if group.paths.is_empty() && asks {
                &root[..]
            } else {
                &group.paths[..]
            };

            for path in paths {
                connected.push(GroupPath {
                    prefix: group_prefix(path),
                    classes: group.client_classes.clone(),
                    hosts: group.hosts.clone(),
                    methods: group.methods.clone(),
                    name: group.name.clone(),
                    pool: Arc::clone(&pool),
                });
            }
        }

        // ADR 0033: a group asking more of a request comes before one
        // asking less, and among groups asking as much the longer prefix
        // wins.

        connected.sort_by_key(|group| {
            (
                std::cmp::Reverse(group.lists()),
                std::cmp::Reverse(group.prefix.len()),
            )
        });

        // Section 6.13: a gRPC mount has no path; its calls are matched by
        // the services its workers registered.

        let mut mounts: Vec<MountPath> = mounts
            .iter()
            .filter(|mount| !mount.path.is_empty())
            .map(|mount| MountPath {
                name: mount.name.clone(),
                path: group_prefix(&mount.path),
                internal: mount.internal,
                accept_redirects_from: mount.accept_redirects_from.clone(),
                accel_redirect: mount.accel_redirect,
                cgi_local_redirect: mount.cgi_local_redirect,
                sendfile: Arc::clone(&mount.sendfile),
            })
            .collect();
        mounts.sort_by_key(|mount| std::cmp::Reverse(mount.path.len()));

        Ok(Self {
            app: app.to_owned(),
            release: release.to_owned(),
            default,
            groups: connected,
            mounts,
            statics: Vec::new(),
            redirects,
            sendfile: Arc::default(),
            errors: Arc::new(ErrorPolicy::default()),
            maintenance: None,
            server: None,
            grpc_web: Arc::default(),
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
            statics: Vec::new(),
            redirects: RedirectFlags::default(),
            sendfile: Arc::default(),
            errors: Arc::new(ErrorPolicy::default()),
            maintenance: None,
            server: None,
            grpc_web: Arc::default(),
        }
    }

    /// The release's own directories of files (section 4.9).
    pub fn with_static_dirs(mut self, statics: Vec<StaticPath>) -> Self {
        self.statics = statics;
        self.statics
            .sort_by_key(|entry| std::cmp::Reverse(entry.prefix.len()));
        self
    }

    /// The same pools with the default mount's file hints by path
    /// (section 4.9).
    pub fn with_sendfile(mut self, sendfile: Arc<Sendfile>) -> Self {
        self.sendfile = sendfile;
        self
    }

    /// The same pools, answering gRPC-Web from these origins' pages with
    /// CORS (section 6.13).
    pub fn with_grpc_web(mut self, origins: &[String]) -> Self {
        self.grpc_web = Arc::new(crate::grpc::WebOrigins::new(origins));
        self
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

    /// Where a path goes: the group asking the most of the request that
    /// it satisfies, by class, host and method (section 4.14 and ADR
    /// 0033), with the most specific path among those, else the default
    /// group, and the longest mount prefix, else the default mount
    /// (section 6.7). An internal mount (section 4.10) is reached only
    /// by a redirect from the release's own application, or from one it
    /// accepts: a client's request for it, or an unaccepted redirect,
    /// gets nothing.
    pub fn select(
        &self,
        path: &str,
        client: &Client<'_>,
        origin: Option<&Origin>,
    ) -> Option<Selection> {
        if let Some(server) = &self.server {
            return Some(Selection {
                pool: Arc::new(LinkPool::empty()),
                app: self.app.clone(),
                release: self.release.clone(),
                group: SERVER_GROUP.to_owned(),
                mount: String::new(),
                script_name: String::new(),
                redirects: RedirectFlags::default(),
                sendfile: Arc::clone(&self.sendfile),
                maintenance: self.maintenance.clone(),
                upstream: Some(Arc::clone(server)),
                grpc_web: Arc::default(),
                static_path: None,
            });
        }

        let carried = client.host.map(normalise);
        let (group, pool) = self
            .groups
            .iter()
            .find(|group| {
                prefix_matches(&group.prefix, path) && group.takes(client, carried.as_deref())
            })
            .map(|group| (group.name.clone(), Arc::clone(&group.pool)))
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

        // Section 4.9: a path under one of the release's own directories
        // is tried as a file first, the workers taking what none answers.

        let static_path = self
            .statics
            .iter()
            .find(|entry| prefix_matches(&entry.prefix, path))
            .cloned();

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
            sendfile: mount.map_or_else(
                || Arc::clone(&self.sendfile),
                |mount| Arc::clone(&mount.sendfile),
            ),
            maintenance: self.maintenance.clone(),
            upstream: None,
            grpc_web: Arc::clone(&self.grpc_web),
            static_path,
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

    /// Section 7.4: the same pools, with each named group that has no
    /// workers waiting to be woken instead of refusing requests.
    pub fn with_waking(mut self, groups: &[String]) -> Self {
        let key = |group: &str| (self.app.clone(), self.release.clone(), group.to_owned());

        if self.default.is_empty() && groups.iter().any(|name| name == DEFAULT_GROUP) {
            self.default = Arc::new(LinkPool::waking(key(DEFAULT_GROUP)));
        }

        let mut waking: Vec<(String, Arc<LinkPool>)> = Vec::new();

        for group in &mut self.groups {
            if !group.pool.is_empty() || !groups.contains(&group.name) {
                continue;
            }

            let pool = match waking.iter().find(|(name, _)| name == &group.name) {
                Some((_, pool)) => Arc::clone(pool),
                None => {
                    let pool = Arc::new(LinkPool::waking(key(&group.name)));
                    waking.push((group.name.clone(), Arc::clone(&pool)));
                    pool
                }
            };

            group.pool = pool;
        }

        self
    }

    /// Every group's pool once, by group name, the default group first.
    pub fn pools(&self) -> Vec<(String, Arc<LinkPool>)> {
        let mut pools = vec![(DEFAULT_GROUP.to_owned(), Arc::clone(&self.default))];

        for group in &self.groups {
            if !pools.iter().any(|(seen, _)| seen == &group.name) {
                pools.push((group.name.clone(), Arc::clone(&group.pool)));
            }
        }

        pools
    }
}

/// The name of the group a release's own settings size (section 7.1).
pub const DEFAULT_GROUP: &str = "default";

/// A group path as a route-style prefix: a trailing `*` means anything
/// below, which is what a prefix already means.
pub fn group_prefix(path: &str) -> String {
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
/// A worker a snapshot named that the edge could not connect to, so the
/// snapshot was applied without it (section 5.1).
#[derive(Debug, Clone)]
pub struct UnreachableWorker {
    /// The application the worker serves.
    pub app: String,
    /// Its release.
    pub release: String,
    /// The worker's link socket.
    pub socket: std::path::PathBuf,
    /// Why it could not be connected to.
    pub reason: String,
}

/// Everything a snapshot gives the edge to route by: the hosts and
/// routes, the applications' pools, the upstreams and the policies.
pub struct Routing {
    version: u64,
    trust: Trust,
    /// The workers left out of the pools, for the daemon to act on.
    unreachable: Vec<UnreachableWorker>,
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
    /// The client classification rules (section 4.14).
    classifier: crate::classify::Classifier,
    /// With no host declared, the default application and the policy
    /// that chooses among its releases, when it has one (section 9.3).
    default_policy: Option<(String, Policy)>,
    /// The application the default workers belong to, when a snapshot
    /// names it, so a preview can reach its other releases (a held one,
    /// section 9.2) without a host declared.
    default_app: Option<String>,
    /// Section 6.13: the listeners that limit cleartext HTTP/2 to some
    /// clients.
    h2c: HashMap<SocketAddr, crate::proxy::Networks>,
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
        /// How the route's uploads are received (section 4.11).
        upload: Option<Arc<crate::uploads::Policy>>,
        /// The route's body limit, in place of the edge's (section 4.6).
        max_body_size: Option<u64>,
        /// The host's and the route's rate limits (section 4.15).
        limits: Vec<Arc<crate::ratelimit::Limit>>,
        /// The methods the route refuses.
        methods: Option<Arc<crate::ratelimit::Methods>>,
        /// The route's site mode.
        mode: Option<Arc<str>>,
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
        /// The host's and the route's rate limits (section 4.15).
        limits: Vec<Arc<crate::ratelimit::Limit>>,
    },
}

impl Decision {
    /// The rate limits the request draws from: the host's, then the
    /// route's.
    pub fn limits(&self) -> &[Arc<crate::ratelimit::Limit>] {
        match self {
            Self::Serve { limits, .. } | Self::Static { limits, .. } => limits,
            _ => &[],
        }
    }
}

impl Routing {
    /// The workers the snapshot named that could not be connected to.
    pub fn unreachable(&self) -> &[UnreachableWorker] {
        &self.unreachable
    }

    /// One application and nothing else: development mode, or a daemon
    /// with no host declared.
    pub fn single(pools: AppPools) -> Self {
        Self {
            version: 0,
            trust: Trust::default(),
            unreachable: Vec::new(),
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
            classifier: crate::classify::Classifier::default(),
            default_policy: None,
            default_app: None,
            h2c: HashMap::new(),
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
            .with_maintenance(maintenance)
            .with_sendfile(Sendfile::new(&app.sendfile_roots, &app.sendfile_header))
            .with_waking(&app.waking_groups)
            .with_grpc_web(&app.grpc_web_origins)
            .with_static_dirs(static_paths_of(app)?);

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

        let default_name = if snapshot.default_app.is_empty() {
            snapshot
                .apps
                .iter()
                .find(|app| app.sockets == snapshot.worker_sockets)
                .map(|app| app.app.clone())
        } else {
            Some(snapshot.default_app.clone())
        }
        .filter(|name| apps.contains_key(name));

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

        let default_policy = match &snapshot.default_traffic {
            Some(traffic) if apps.contains_key(&snapshot.default_app) => Some((
                snapshot.default_app.clone(),
                Policy::from_message(traffic)
                    .map_err(|error| anyhow::anyhow!("the default application: {error}"))?,
            )),
            _ => None,
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

        // Sections 4.14 and 4.15: the classes and the rate limits hosts
        // and routes name. A name with no limit is left out, with a
        // warning, rather than refusing the snapshot.

        let classifier = crate::classify::Classifier::from_messages(&snapshot.client_classes)
            .map_err(|error| anyhow::anyhow!(error))?;
        let mut limits: HashMap<String, Arc<crate::ratelimit::Limit>> = HashMap::new();

        for message in &snapshot.rate_limits {
            let limit = crate::ratelimit::Limit::from_message(message)
                .map_err(|error| anyhow::anyhow!(error))?;
            limits.insert(message.name.clone(), limit);
        }

        let named_limits = |names: &[String], owner: &str| -> Vec<Arc<crate::ratelimit::Limit>> {
            names
                .iter()
                .filter_map(|name| {
                    let found = limits.get(name).cloned();

                    if found.is_none() {
                        tracing::warn!(%owner, limit = %name, "no rate limit by that name; it is not applied");
                    }

                    found
                })
                .collect()
        };

        let mut h2c = HashMap::new();

        for policy in &snapshot.h2c {
            let listener: SocketAddr = policy
                .listener
                .parse()
                .map_err(|_| anyhow::anyhow!("h2c: {:?} is not IP:PORT", policy.listener))?;
            let networks = crate::proxy::Networks::parse(&policy.allowed)
                .map_err(|error| anyhow::anyhow!("listener {listener}: h2c: {error}"))?;
            h2c.insert(listener, networks);
        }

        // Section 5.1: a worker that could not be connected to is reported
        // rather than refusing the snapshot.

        let mut unreachable = Vec::new();

        for (name, releases) in &apps {
            for (release, pools) in &releases.releases {
                for worker in pools.unreachable() {
                    unreachable.push(UnreachableWorker {
                        app: name.clone(),
                        release: release.clone(),
                        socket: worker.path.clone(),
                        reason: worker.reason.clone(),
                    });
                }
            }
        }

        if default_app.is_none() {
            for worker in default.unreachable() {
                unreachable.push(UnreachableWorker {
                    app: snapshot.default_app.clone(),
                    release: snapshot.default_release.clone(),
                    socket: worker.path.clone(),
                    reason: worker.reason.clone(),
                });
            }
        }

        let mut routing = Self {
            version: snapshot.version,
            trust,
            unreachable,
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
            classifier,
            default_policy,
            default_app: default_name,
            h2c,
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
                rate_limits: named_limits(&host.rate_limits, &format!("host {}", host.name)),
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
                    classes: route.client_classes.clone(),
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
                    upload: route
                        .upload
                        .as_ref()
                        .and_then(crate::uploads::Policy::from_message),
                    max_body_size: (route.max_body_size > 0).then_some(route.max_body_size),
                    rate_limits: named_limits(
                        &route.rate_limits,
                        &format!("route {} {}", route.host, route.path),
                    ),
                    methods: route
                        .methods
                        .as_ref()
                        .and_then(crate::ratelimit::Methods::from_message),
                    mode: (!route.mode.is_empty()).then(|| Arc::from(route.mode.as_str())),
                });
            }
        }

        // Section 4.14: of routes with the same path, those for client
        // classes come before the one for every request.

        for routes in &mut routing.routes {
            routes.sort_by_key(|route| {
                (
                    std::cmp::Reverse(route.path.len()),
                    route.classes.is_empty(),
                )
            });
        }

        Ok(routing)
    }

    /// The class of a request (section 4.14).
    pub fn classify(
        &self,
        client: std::net::IpAddr,
        headers: &HeaderMap,
        ja4: Option<&str>,
        verifier: &Arc<crate::classify::Verifier>,
    ) -> Arc<str> {
        self.classifier.classify(client, headers, ja4, verifier)
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
        class: &str,
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
                .find(|route| route.matches(path, class))
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

    /// Section 6.13: whether a client may speak HTTP/2 in cleartext on
    /// the listener at `local`.
    pub fn allows_h2c(&self, local: &SocketAddr, peer: &SocketAddr) -> bool {
        self.h2c
            .get(local)
            .is_none_or(|networks| networks.contains(peer))
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

    /// The same tables trusting these proxies, for a development run,
    /// which has no snapshot to carry them.
    pub fn with_trust(mut self, trust: Trust) -> Self {
        self.trust = trust;
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

    /// The default application's pools, by group name, the default group
    /// first: what a development run reports when it starts.
    pub fn default_pools(&self) -> Vec<(String, Arc<LinkPool>)> {
        self.default.pools()
    }

    /// Section 7.4: every named release's groups with their pools, for
    /// handing woken groups to the requests waiting for them.
    pub fn group_pools(&self) -> Vec<(crate::wake::Key, Arc<LinkPool>)> {
        self.app_pools()
            .iter()
            .filter(|pools| !pools.labels().0.is_empty())
            .flat_map(|pools| {
                let (app, release) = pools.labels();
                pools
                    .pools()
                    .into_iter()
                    .map(|(group, pool)| ((app.to_owned(), release.to_owned(), group), pool))
            })
            .collect()
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
            // A rollout of the default application chooses among its
            // releases as a route's policy does, and without one a
            // preview still reaches a held release of it.

            let mut cookies = Vec::new();

            let selection = match (&self.default_policy, &self.default_app) {
                (Some((app, policy)), _) => {
                    self.selection_for(app, Some(policy), path, client, &mut cookies, origin)
                }
                (None, Some(app)) => {
                    self.selection_for(app, None, path, client, &mut cookies, origin)
                }
                (None, None) => self.default.select(path, client, origin),
            };

            return match selection {
                Some(selection) => Decision::Serve {
                    selection,
                    server_name: None,
                    cookies,
                    auth: None,
                    upload: None,
                    max_body_size: None,
                    limits: Vec::new(),
                    methods: None,
                    mode: None,
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

        // ADR 0033: a worker group may be for the host by its configured
        // name as well as by the name the request carries.

        let client = &Client {
            virtual_host: Some(&host.name),
            ..*client
        };

        if host.https_redirect && !tls && origin.is_none() {
            return Decision::Redirect;
        }

        let Some(route) = self.routes[index]
            .iter()
            .find(|route| route.matches(path, client.client_class))
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
                limits: host
                    .rate_limits
                    .iter()
                    .chain(&route.rate_limits)
                    .cloned()
                    .collect(),
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
            upload: route.upload.clone(),
            max_body_size: route.max_body_size,
            limits: host
                .rate_limits
                .iter()
                .chain(&route.rate_limits)
                .cloned()
                .collect(),
            methods: route.methods.clone(),
            mode: route.mode.clone(),
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
            return self.default.select(path, client, Some(&origin));
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
                    Some(pools) => pools.select(path, client, origin),
                    None => choose(releases, policy, client, cookies, &self.upstreams)
                        .select(path, client, origin),
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
                sendfile: Arc::default(),
                maintenance: None,
                upstream: None,
                grpc_web: Arc::default(),
                static_path: None,
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

/// A release's own directories of files, from its snapshot entry.
fn static_paths_of(app: &pb::AppWorkers) -> anyhow::Result<Vec<StaticPath>> {
    app.static_dirs
        .iter()
        .map(|entry| {
            Ok(StaticPath {
                prefix: group_prefix(&entry.path),
                route: Arc::new(
                    crate::static_files::StaticRoute::from_static_dir(entry).map_err(|error| {
                        anyhow::anyhow!(
                            "{}@{}: static {}: {error}",
                            app.app,
                            app.release,
                            entry.path
                        )
                    })?,
                ),
            })
        })
        .collect()
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
            sendfile: Sendfile::new(&mount.sendfile_roots, &mount.sendfile_header),
        })
        .collect()
}

fn groups_of(app: &pb::AppWorkers) -> Vec<GroupSockets> {
    app.groups
        .iter()
        .map(|group| GroupSockets {
            name: group.name.clone(),
            paths: group.paths.clone(),
            client_classes: group.client_classes.clone(),
            hosts: group.hosts.clone(),
            methods: group.methods.clone(),
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
            h2c: vec![pb::H2cPolicy {
                listener: "0.0.0.0:8080".to_owned(),
                allowed: vec!["10.0.0.0/8".to_owned(), "loopback".to_owned()],
            }],
            client_classes: Vec::new(),
            rate_limits: Vec::new(),
            default_traffic: None,
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

    #[test]
    fn a_listener_may_keep_cleartext_http2_to_some_clients() {
        let routing = routing();
        let limited: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let open: SocketAddr = "0.0.0.0:8081".parse().unwrap();
        let inside: SocketAddr = "10.1.2.3:50000".parse().unwrap();
        let outside: SocketAddr = "203.0.113.9:50000".parse().unwrap();
        assert!(routing.allows_h2c(&limited, &inside));
        assert!(routing.allows_h2c(&limited, &"127.0.0.1:1".parse().unwrap()));
        assert!(!routing.allows_h2c(&limited, &outside));
        assert!(routing.allows_h2c(&open, &outside));
    }

    /// A request with these headers, from a fixed client.
    fn client<'a>(headers: &'a HeaderMap, ids: &'a crate::errors::RequestIds) -> Client<'a> {
        Client {
            headers,
            peer: "10.0.0.1".parse().unwrap(),
            secure: false,
            ids,
            client_class: "default",
            host: None,
            virtual_host: None,
            method: "GET",
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
            classes: Default::default(),
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

    /// The application, release and group a request of a class to a
    /// path of shop.example.com goes to.
    fn placed(routing: &Routing, path: &str, class: &str) -> (String, String, String) {
        let headers = HeaderMap::new();
        let ids = crate::errors::RequestIds::new();
        let client = Client {
            client_class: class,
            ..client(&headers, &ids)
        };

        match routing.select(
            Some("shop.example.com"),
            path,
            "127.0.0.1:1",
            true,
            &client,
            None,
        ) {
            Decision::Serve { selection, .. } => {
                (selection.app, selection.release, selection.group)
            }
            _ => panic!("{path} as {class} is not served"),
        }
    }

    /// The group a request of the method, carrying the host, lands in.
    fn grouped(routing: &Routing, path: &str, host: &str, method: &str) -> String {
        let headers = HeaderMap::new();
        let ids = crate::errors::RequestIds::new();
        let client = Client {
            host: Some(host),
            method,
            ..client(&headers, &ids)
        };

        match routing.select(Some(host), path, "127.0.0.1:1", true, &client, None) {
            Decision::Serve { selection, .. } => selection.group,
            _ => panic!("{method} {path} on {host} is not served"),
        }
    }

    #[test]
    fn hosts_and_methods_pick_groups() {
        // ADR 0033: a group asking more of a request comes first, a host
        // is matched by the virtual host's name or the name carried, and
        // a method in upper case.

        let mut snapshot = split_snapshot(true, "");
        let groups = vec![
            pb::Group {
                name: "files".to_owned(),
                paths: vec!["/files".to_owned()],
                ..Default::default()
            },
            pb::Group {
                name: "uploads".to_owned(),
                paths: vec!["/files".to_owned()],
                methods: vec!["POST".to_owned(), "PUT".to_owned()],
                ..Default::default()
            },
            pb::Group {
                name: "both".to_owned(),
                paths: vec!["/files/big".to_owned()],
                hosts: vec!["shop".to_owned()],
                methods: vec!["POST".to_owned()],
                ..Default::default()
            },
            pb::Group {
                name: "byname".to_owned(),
                paths: vec!["/reports".to_owned()],
                hosts: vec!["shop".to_owned()],
                ..Default::default()
            },
            pb::Group {
                name: "carried".to_owned(),
                paths: vec!["/carried".to_owned()],
                hosts: vec!["shop.example.com".to_owned()],
                ..Default::default()
            },
        ];

        // The split sends a request to either release, so both carry
        // the groups.

        for app in &mut snapshot.apps {
            app.groups = groups.clone();
        }

        let routing = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(Routing::from_snapshot(&snapshot))
            .unwrap();
        let at = |path: &str, host: &str, method: &str| grouped(&routing, path, host, method);

        assert_eq!(at("/files/x", "shop.example.com", "GET"), "files");
        assert_eq!(at("/files/x", "shop.example.com", "POST"), "uploads");
        assert_eq!(at("/files/x", "shop.example.com", "put"), "uploads");
        assert_eq!(at("/files/big/x", "shop.example.com", "POST"), "both");
        assert_eq!(at("/files/big/x", "shop.example.com", "GET"), "files");
        assert_eq!(at("/reports/x", "shop.example.com", "GET"), "byname");
        assert_eq!(at("/carried/x", "shop.example.com", "GET"), "carried");
        assert_eq!(at("/carried/x", "Shop.Example.com:443", "GET"), "carried");
        assert_eq!(at("/", "shop.example.com", "GET"), "default");
    }

    #[test]
    fn client_classes_pick_routes_releases_and_groups() {
        let mut snapshot = split_snapshot(true, "");
        snapshot.routes[0].traffic.as_mut().unwrap().classes =
            [("ai_crawler".to_owned(), "r2".to_owned())]
                .into_iter()
                .collect();
        snapshot.routes[0].traffic.as_mut().unwrap().shares[0].weight = 0;
        snapshot.routes.push(pb::Route {
            host: "shop".to_owned(),
            path: "/".to_owned(),
            app: "archive".to_owned(),
            client_classes: vec!["partner".to_owned()],
            ..Default::default()
        });
        snapshot.apps[0].groups = vec![
            pb::Group {
                name: "hot".to_owned(),
                paths: vec!["/status".to_owned()],
                ..Default::default()
            },
            pb::Group {
                name: "crawlers".to_owned(),
                client_classes: vec!["ai_crawler".to_owned()],
                ..Default::default()
            },
        ];
        let routing = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(Routing::from_snapshot(&snapshot))
            .unwrap();
        let at = |path: &str, class: &str| placed(&routing, path, class);

        // A route for a class comes before the one for every request at
        // the same path, and a longer path still wins.

        assert_eq!(at("/", "partner").0, "archive");
        assert_eq!(at("/", "default").0, "shop");
        assert_eq!(at("/api/x", "partner").0, "api");

        // The policy gives a class its release, ahead of the split, which
        // gives r2 nothing.

        assert_eq!(at("/", "default").1, "r1");
        assert_eq!(at("/", "ai_crawler").1, "r2");

        // A group for a class takes its requests on every path, ahead of
        // a group for the path; others keep to the path's group.

        assert_eq!(at("/status", "default").2, "hot");
        assert_eq!(at("/", "default").2, "default");

        // Without the policy, which sends the class to r2, its requests
        // reach r1's group for it.

        snapshot.routes[0].traffic = None;
        let routing = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(Routing::from_snapshot(&snapshot))
            .unwrap();
        assert_eq!(placed(&routing, "/status", "ai_crawler").2, "crawlers");
        assert_eq!(placed(&routing, "/", "ai_crawler").2, "crawlers");
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
