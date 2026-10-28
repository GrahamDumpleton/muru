//! Authentication sub-requests (section 4.16): before the edge serves a
//! guarded route, it asks an application path whether the client may
//! have the request, with a bodiless GET that carries the client's
//! headers. 2xx allows it, and listed headers of the answer travel on to
//! the target as trusted headers; 401, 403 and redirects go back to the
//! client as the handler answered them; anything else, a timeout or an
//! unreachable handler is 503 `auth_unavailable`, so the check fails
//! closed. Allowing decisions can be remembered for a while, keyed by
//! listed cookies and headers.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use muru_proto::edge::v1 as pb;

use crate::ResponseBody;
use crate::errors::Problem;
use crate::link::{self, Context, Handled, Internal, Original};
use crate::routing::{Client, RedirectFlags, Routing};

/// The reason a request the handler refused is logged with: the response
/// is the handler's own, with its status.
pub const DENIED_REASON: &str = "auth_denied";

/// The reason the 503 for a handler that could not decide carries.
pub const UNAVAILABLE_REASON: &str = "auth_unavailable";

/// How long a handler gets, queueing included, unless the route says.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// The most decisions one route remembers.
const CACHE_CAPACITY: usize = 10_000;

/// The most of a refusing handler's body that is passed to the client.
const MAX_BODY_BYTES: usize = 1 << 20;

/// The protected request's method, on the sub-request.
pub static X_ORIGINAL_METHOD: HeaderName = HeaderName::from_static("x-original-method");

/// The protected request's path and query, on the sub-request.
pub static X_ORIGINAL_URI: HeaderName = HeaderName::from_static("x-original-uri");

/// The host the protected request asked for, on the sub-request.
pub static X_ORIGINAL_HOST: HeaderName = HeaderName::from_static("x-original-host");

/// A route's compiled check.
#[derive(Debug)]
pub struct Auth {
    /// The application asked; empty when an upstream is.
    pub app: String,
    /// The upstream asked (section 4.13), instead of an application.
    pub upstream: String,
    /// The path asked.
    pub path: String,
    /// The handler's budget, queueing included.
    pub timeout: Duration,
    copy_headers: Vec<HeaderName>,
    cache: Option<Cache>,
}

/// What a remembered decision is keyed by.
#[derive(Debug)]
enum Key {
    Cookie(String),
    Header(HeaderName),
}

/// The allowing decisions a route remembers, per edge process.
#[derive(Debug)]
struct Cache {
    ttl: Duration,
    keys: Vec<Key>,
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug, Clone)]
struct Entry {
    expires: Instant,
    copied: Vec<(HeaderName, HeaderValue)>,
}

/// What the check decided.
pub enum Outcome {
    /// The handler allowed the request, with the headers to add to it;
    /// `result` is `allowed`, or `cached` when the handler was not asked.
    Allowed {
        /// The listed headers of the handler's answer.
        copied: Vec<(HeaderName, HeaderValue)>,
        /// For the metrics.
        result: &'static str,
    },
    /// The handler refused or redirected: its answer, for the client.
    Denied(Response<ResponseBody>),
    /// The handler could not decide: why, for the edge's log.
    Unavailable(String),
}

impl Auth {
    /// Compile a route's check from the snapshot.
    pub fn from_message(message: &pb::Auth) -> Result<Self, String> {
        if (message.app.is_empty() && message.upstream.is_empty()) || !message.path.starts_with('/')
        {
            return Err("auth: an application and a path starting with / are needed".to_owned());
        }

        let copy_headers = message
            .copy_headers
            .iter()
            .map(|name| {
                HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes())
                    .map_err(|_| format!("auth: copy_headers: {name:?} is not a header name"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let cache = if message.cache_ttl_ms == 0 {
            None
        } else {
            let keys = message
                .cache_key
                .iter()
                .map(|key| match key.split_once(':') {
                    Some(("cookie", name)) if !name.is_empty() => Ok(Key::Cookie(name.to_owned())),
                    Some(("header", name)) if !name.is_empty() => {
                        HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes())
                            .map(Key::Header)
                            .map_err(|_| format!("auth: cache.key: {name:?} is not a header name"))
                    }
                    _ => Err(format!(
                        "auth: cache.key: {key:?} must be cookie:NAME or header:NAME"
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;

            if keys.is_empty() {
                return Err("auth: cache.key names nothing".to_owned());
            }

            Some(Cache {
                ttl: Duration::from_millis(message.cache_ttl_ms),
                keys,
                entries: Mutex::new(HashMap::new()),
            })
        };

        Ok(Self {
            app: message.app.clone(),
            upstream: message.upstream.clone(),
            path: message.path.clone(),
            timeout: if message.timeout_ms == 0 {
                DEFAULT_TIMEOUT
            } else {
                Duration::from_millis(message.timeout_ms)
            },
            copy_headers,
            cache,
        })
    }

    /// Remove a client's own copies of the headers the handler supplies,
    /// and of the ones that describe the original request, so only the
    /// edge ever sets them.
    pub fn strip(&self, headers: &mut HeaderMap) {
        for name in &self.copy_headers {
            headers.remove(name);
        }

        headers.remove(&X_ORIGINAL_METHOD);
        headers.remove(&X_ORIGINAL_URI);
        headers.remove(&X_ORIGINAL_HOST);
    }

    /// The listed headers of an allowing answer.
    fn copied(&self, headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
        self.copy_headers
            .iter()
            .flat_map(|name| {
                headers
                    .get_all(name)
                    .iter()
                    .map(move |value| (name.clone(), value.clone()))
            })
            .collect()
    }

    /// The cache key of a request: the listed cookies and headers it
    /// carries. None without a cache, or when it carries none of them,
    /// since an anonymous request is never served from the cache.
    fn key_for(&self, headers: &HeaderMap) -> Option<String> {
        let cache = self.cache.as_ref()?;
        let mut key = String::new();
        let mut present = false;

        for entry in &cache.keys {
            let value = match entry {
                Key::Cookie(name) => crate::routing::cookie(headers, name),
                Key::Header(name) => headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            };

            if let Some(value) = value {
                present = true;
                key.push_str(&value);
            }

            key.push('\u{0}');
        }

        present.then_some(key)
    }

    /// A remembered decision for the key, while it stands.
    fn remembered(&self, key: &str) -> Option<Vec<(HeaderName, HeaderValue)>> {
        let cache = self.cache.as_ref()?;
        let entries = cache.entries.lock().expect("auth cache poisoned");
        entries
            .get(key)
            .filter(|entry| entry.expires > Instant::now())
            .map(|entry| entry.copied.clone())
    }

    /// Remember an allowing decision. The cache is bounded: when it is
    /// full, expired entries go first, and a decision that still finds
    /// no room is simply not remembered.
    fn remember(&self, key: String, copied: &[(HeaderName, HeaderValue)]) {
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        let now = Instant::now();
        let mut entries = cache.entries.lock().expect("auth cache poisoned");

        if entries.len() >= CACHE_CAPACITY && !entries.contains_key(&key) {
            entries.retain(|_, entry| entry.expires > now);
        }

        if entries.len() >= CACHE_CAPACITY && !entries.contains_key(&key) {
            return;
        }

        entries.insert(
            key,
            Entry {
                expires: now + cache.ttl,
                copied: copied.to_vec(),
            },
        );
    }
}

/// What a check needs from the request it guards.
pub struct Check<'a> {
    /// The routing tables in force.
    pub routing: &'a Routing,
    /// The guarded request, whose headers the sub-request carries.
    pub parts: &'a http::request::Parts,
    /// The guarded request's context; the sub-request's is derived from
    /// it.
    pub context: &'a Context<'a>,
    /// The sub-request's own identifier.
    pub request_id: &'a str,
    /// What the release choice for the handler's application sees.
    pub client: &'a Client<'a>,
}

/// Ask the route's handler about the request, unless a remembered
/// decision answers.
pub async fn check(auth: &Auth, check: Check<'_>) -> Outcome {
    let key = auth.key_for(&check.parts.headers);

    if let Some(copied) = key.as_deref().and_then(|key| auth.remembered(key)) {
        return Outcome::Allowed {
            copied,
            result: "cached",
        };
    }

    // ADR 0033: the sub-request's own method, HEAD or GET, is what a
    // worker group of the authentication application may be for.

    let method = if check.parts.method == Method::HEAD {
        "HEAD"
    } else {
        "GET"
    };
    let client = crate::routing::Client {
        method,
        ..*check.client
    };

    let selection = if auth.upstream.is_empty() {
        check.routing.select_app(&auth.app, &auth.path, &client)
    } else {
        check.routing.select_upstream(&auth.upstream)
    };
    let Some(selection) = selection else {
        return Outcome::Unavailable(format!("{}: the path's mount is not reachable", auth.path));
    };

    // The sub-request: a GET for the handler's path, with the client's
    // headers and no body, and the original request described in the
    // head for the application and in headers for an HTTP upstream.

    let target = check.parts.uri.path_and_query().map_or_else(
        || check.parts.uri.path().to_owned(),
        |target| target.to_string(),
    );
    let host = check
        .context
        .server_name
        .clone()
        .unwrap_or_else(|| link::requested_host(check.parts));
    let (mut sub, _) = http::Request::new(()).into_parts();
    sub.method = if check.parts.method == Method::HEAD {
        Method::HEAD
    } else {
        Method::GET
    };
    sub.uri = auth.path.parse().unwrap_or_else(|_| Uri::from_static("/"));
    sub.version = check.parts.version;
    sub.headers = check.parts.headers.clone();
    sub.headers.remove(http::header::CONTENT_LENGTH);
    sub.headers.remove(http::header::CONTENT_TYPE);
    sub.headers.remove(http::header::EXPECT);
    sub.headers.remove(http::header::UPGRADE);
    sub.headers.remove(http::header::SEC_WEBSOCKET_KEY);
    sub.headers.remove(http::header::SEC_WEBSOCKET_VERSION);
    sub.headers.remove(http::header::SEC_WEBSOCKET_PROTOCOL);
    sub.headers.remove(http::header::SEC_WEBSOCKET_EXTENSIONS);

    for (name, value) in [
        (&X_ORIGINAL_METHOD, check.parts.method.as_str()),
        (&X_ORIGINAL_URI, target.as_str()),
        (&X_ORIGINAL_HOST, host.as_str()),
    ] {
        if let Ok(value) = HeaderValue::from_str(value) {
            sub.headers.insert(name.clone(), value);
        }
    }

    let internal = Internal {
        redirect_url: check.parts.uri.path().to_owned(),
        redirect_query_string: check.parts.uri.query().unwrap_or_default().to_owned(),
        redirect_status: 0,
        headers: Vec::new(),
        hop: check.context.hops.len() as u32,
        error_reason: String::new(),
        original: Some(Original {
            method: check.parts.method.as_str().to_owned(),
            uri: target.clone(),
            host,
        }),
    };
    let mut context = check
        .context
        .sub_request(check.request_id, auth.timeout, internal, target);
    context.app = selection.app;
    context.release = selection.release;
    context.group = selection.group;
    context.mount = selection.mount;
    context.mount_path = selection.script_name;

    let handled = crate::dispatch(
        &mut sub,
        None,
        None,
        selection.pool,
        selection.upstream,
        &context,
        RedirectFlags::default(),
    )
    .await;

    let response = match handled {
        Handled::Response(response) => response,
        Handled::Redirect(_) => {
            return Outcome::Unavailable(format!(
                "{}: the handler redirected internally",
                auth.path
            ));
        }
    };

    let status = response.status();

    // 2xx: allowed, with the listed headers. The rest of the handler's
    // body, if any, drains in the background.

    if status.is_success() {
        let copied = auth.copied(response.headers());

        if let Some(key) = key {
            auth.remember(key, &copied);
        }

        return Outcome::Allowed {
            copied,
            result: "allowed",
        };
    }

    // 401, 403 and redirects: the handler's answer goes to the client.

    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) || status.is_redirection()
    {
        let (head, body) = response.into_parts();

        return match collect(body, auth.timeout).await {
            Some(bytes) => {
                let body = Full::new(bytes)
                    .map_err(|never| match never {})
                    .boxed_unsync();
                let mut response = Response::from_parts(head, body);
                response
                    .headers_mut()
                    .remove(http::header::TRANSFER_ENCODING);
                Outcome::Denied(response)
            }
            None => Outcome::Unavailable(format!(
                "{}: the handler's {status} response could not be read whole",
                auth.path
            )),
        };
    }

    Outcome::Unavailable(format!("{}: the handler answered {status}", auth.path))
}

/// The problem a handler that could not decide is answered with.
pub fn unavailable() -> Problem {
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        UNAVAILABLE_REASON,
        "The authentication handler could not decide whether to allow the request.",
    )
}

/// The problem a refused request is answered with when an error page is
/// configured for the status and reason: the handler's own headers that
/// matter to the client are kept.
pub fn denied(status: StatusCode, headers: &HeaderMap) -> Problem {
    let mut problem = Problem::new(
        status,
        DENIED_REASON,
        "The authentication handler refused the request.",
    );

    for name in [
        http::header::WWW_AUTHENTICATE,
        http::header::SET_COOKIE,
        http::header::LOCATION,
        http::header::RETRY_AFTER,
    ] {
        for value in headers.get_all(&name) {
            problem.headers.push((name.clone(), value.clone()));
        }
    }

    problem
}

/// A handler's body, whole, within the budget and the size cap.
async fn collect(mut body: ResponseBody, budget: Duration) -> Option<Bytes> {
    let deadline = Instant::now() + budget;
    let mut data = BytesMut::new();

    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline.into(), body.frame()).await {
        let Ok(frame) = frame else {
            return None;
        };

        if let Ok(bytes) = frame.into_data() {
            if data.len() + bytes.len() > MAX_BODY_BYTES {
                return None;
            }

            data.extend_from_slice(&bytes);
        }
    }

    Some(data.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(cache_ttl_ms: u64, cache_key: &[&str]) -> Auth {
        Auth::from_message(&pb::Auth {
            app: "shop".to_owned(),
            path: "/_auth".to_owned(),
            timeout_ms: 0,
            copy_headers: vec!["X-Auth-User".to_owned()],
            cache_ttl_ms,
            cache_key: cache_key.iter().map(|key| (*key).to_owned()).collect(),
            upstream: String::new(),
        })
        .expect("a valid check")
    }

    #[test]
    fn the_cache_is_keyed_by_the_listed_cookies_and_headers() {
        let auth = auth(30_000, &["cookie:sessionid", "header:authorization"]);
        let mut headers = HeaderMap::new();
        assert!(
            auth.key_for(&headers).is_none(),
            "anonymous requests are never cached"
        );

        headers.insert(
            http::header::COOKIE,
            HeaderValue::from_static("a=1; sessionid=s1"),
        );
        let key = auth.key_for(&headers).expect("keyed by the cookie");
        let copied = vec![(
            HeaderName::from_static("x-auth-user"),
            HeaderValue::from_static("alice"),
        )];
        auth.remember(key.clone(), &copied);
        assert_eq!(auth.remembered(&key), Some(copied));

        headers.insert(
            http::header::COOKIE,
            HeaderValue::from_static("sessionid=s2"),
        );
        let other = auth.key_for(&headers).expect("keyed by the cookie");
        assert_ne!(key, other);
        assert!(auth.remembered(&other).is_none());
    }

    #[test]
    fn without_a_ttl_nothing_is_remembered() {
        let auth = auth(0, &[]);
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            HeaderValue::from_static("sessionid=s1"),
        );
        assert!(auth.key_for(&headers).is_none());
        assert_eq!(auth.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn a_client_cannot_supply_the_handlers_headers() {
        let auth = auth(0, &[]);
        let mut headers = HeaderMap::new();
        headers.insert("x-auth-user", HeaderValue::from_static("mallory"));
        headers.insert("x-original-uri", HeaderValue::from_static("/admin"));
        headers.insert("x-other", HeaderValue::from_static("kept"));
        auth.strip(&mut headers);
        assert!(headers.get("x-auth-user").is_none());
        assert!(headers.get("x-original-uri").is_none());
        assert!(headers.get("x-other").is_some());
    }

    #[test]
    fn a_bad_key_is_refused() {
        let error = Auth::from_message(&pb::Auth {
            app: "shop".to_owned(),
            path: "/_auth".to_owned(),
            cache_ttl_ms: 1000,
            cache_key: vec!["query:x".to_owned()],
            ..Default::default()
        })
        .expect_err("refused");
        assert!(error.contains("cookie:NAME or header:NAME"), "{error}");
    }
}
