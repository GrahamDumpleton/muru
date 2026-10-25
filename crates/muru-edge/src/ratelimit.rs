//! Rate limits and method restrictions (design specification section
//! 4.15). Rate limits are token buckets keyed
//! by the client's address (to a prefix), its class, a header, a cookie or
//! a combination, which a host's and a route's limits draw from in turn.
//! A bucket fills at the limit's rate up to its burst; a request that
//! finds it empty is refused with 429, held until a token comes when the
//! limit delays, or only counted when it logs. The buckets belong to the
//! edge process and outlive snapshots; idle full ones are dropped.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};
use muru_proto::edge::v1 as pb;

use crate::errors::Problem;
use crate::routing::cookie;

/// How often idle buckets are looked for.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// A limit as a snapshot carries it.
#[derive(Debug)]
pub struct Limit {
    /// The limit's name, as metrics and the RateLimit fields show it.
    pub name: Arc<str>,
    classes: Vec<String>,
    key: Vec<KeyPart>,
    per_second: f64,
    burst: f64,
    action: Action,
    max_delay: Duration,
    ipv4_prefix: u8,
    ipv6_prefix: u8,
    headers: bool,
}

#[derive(Debug)]
enum KeyPart {
    Address,
    Class,
    Header(HeaderName),
    Cookie(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Reject,
    Delay,
    Log,
}

impl Limit {
    /// The limit a snapshot carries.
    pub fn from_message(message: &pb::RateLimit) -> Result<Arc<Self>, String> {
        let mut key = Vec::new();

        for part in &message.key {
            key.push(match part.as_str() {
                "address" => KeyPart::Address,
                "client_class" => KeyPart::Class,
                other => {
                    if let Some(name) = other.strip_prefix("header:") {
                        KeyPart::Header(HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                            format!("rate_limit {}: {name:?} is not a header", message.name)
                        })?)
                    } else if let Some(name) = other.strip_prefix("cookie:") {
                        KeyPart::Cookie(name.to_owned())
                    } else {
                        return Err(format!(
                            "rate_limit {}: unknown key {other:?}",
                            message.name
                        ));
                    }
                }
            });
        }

        if message.per_second <= 0.0 || message.burst == 0 {
            return Err(format!("rate_limit {}: no rate", message.name));
        }

        Ok(Arc::new(Self {
            name: Arc::from(message.name.as_str()),
            classes: message.classes.clone(),
            key,
            per_second: message.per_second,
            burst: f64::from(message.burst),
            action: match message.action.as_str() {
                "delay" => Action::Delay,
                "log" => Action::Log,
                _ => Action::Reject,
            },
            max_delay: Duration::from_millis(message.max_delay_ms),
            ipv4_prefix: message.ipv4_prefix.clamp(1, 32) as u8,
            ipv6_prefix: message.ipv6_prefix.clamp(1, 128) as u8,
            headers: message.headers,
        }))
    }

    fn applies(&self, class: &str) -> bool {
        self.classes.is_empty() || self.classes.iter().any(|name| name == class)
    }

    /// The bucket a request draws from.
    fn key(&self, client: IpAddr, class: &str, headers: &HeaderMap) -> String {
        let mut key = String::new();

        for (index, part) in self.key.iter().enumerate() {
            if index > 0 {
                key.push('\u{1f}');
            }

            match part {
                KeyPart::Address => {
                    key.push_str(&masked(client, self.ipv4_prefix, self.ipv6_prefix))
                }
                KeyPart::Class => key.push_str(class),
                KeyPart::Header(name) => {
                    key.push_str(
                        headers
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or(""),
                    );
                }
                KeyPart::Cookie(name) => key.push_str(&cookie(headers, name).unwrap_or_default()),
            }
        }

        key
    }

    /// Seconds for an empty bucket to fill again: the window of the
    /// RateLimit-Policy field.
    fn window(&self) -> u64 {
        (self.burst / self.per_second).ceil() as u64
    }
}

/// An address with only its leading `bits` kept, as text.
fn masked(address: IpAddr, ipv4_prefix: u8, ipv6_prefix: u8) -> String {
    match address.to_canonical() {
        IpAddr::V4(ip) => {
            let mask = u32::MAX
                .checked_shl(32 - u32::from(ipv4_prefix))
                .unwrap_or(0);
            format!(
                "{}/{ipv4_prefix}",
                std::net::Ipv4Addr::from(u32::from(ip) & mask)
            )
        }
        IpAddr::V6(ip) => {
            let mask = u128::MAX
                .checked_shl(128 - u32::from(ipv6_prefix))
                .unwrap_or(0);
            format!(
                "{}/{ipv6_prefix}",
                std::net::Ipv6Addr::from(u128::from(ip) & mask)
            )
        }
    }
}

/// What a request's limits decided.
#[derive(Debug)]
pub enum Verdict {
    /// Serve it now.
    Allow,
    /// Serve it after the wait.
    Delay(Duration),
    /// Refuse it with 429.
    Reject {
        /// The limit that refused.
        limit: Arc<Limit>,
        /// Seconds until the bucket has a token again.
        retry_after: u64,
    },
}

/// The outcome of a request's limits: the verdict and, for the metrics,
/// each limit that did not simply allow it, with what it did.
#[derive(Debug)]
pub struct Outcome {
    /// What happens to the request.
    pub verdict: Verdict,
    /// Limit names with `rejected`, `delayed` or `logged`.
    pub counted: Vec<(Arc<str>, &'static str)>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
    per_second: f64,
    burst: f64,
}

impl Bucket {
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.burst);
        self.at = now;
    }

    /// Seconds until a token is there; 0 when one is.
    fn wait(&self) -> f64 {
        if self.tokens >= 1.0 {
            0.0
        } else {
            (1.0 - self.tokens) / self.per_second
        }
    }
}

/// Every limit's buckets, for the edge process.
#[derive(Debug)]
pub struct Buckets {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    buckets: HashMap<(Arc<str>, String), Bucket>,
    swept: Instant,
}

impl Default for Buckets {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                buckets: HashMap::new(),
                swept: Instant::now(),
            }),
        }
    }
}

impl Buckets {
    /// Draw from each applicable limit's bucket in turn: the first that
    /// refuses decides, and the longest delay is the wait.
    pub fn check(
        &self,
        limits: &[Arc<Limit>],
        class: &str,
        client: IpAddr,
        headers: &HeaderMap,
        now: Instant,
    ) -> Outcome {
        let mut state = self.state.lock().expect("buckets poisoned");
        let mut delay = Duration::ZERO;
        let mut counted = Vec::new();

        if now.saturating_duration_since(state.swept) >= SWEEP_EVERY {
            state.buckets.retain(|_, bucket| {
                let mut idle = *bucket;
                idle.refill(now);
                idle.tokens < idle.burst
            });
            state.swept = now;
        }

        for limit in limits.iter().filter(|limit| limit.applies(class)) {
            let key = (Arc::clone(&limit.name), limit.key(client, class, headers));
            let bucket = state.buckets.entry(key).or_insert(Bucket {
                tokens: limit.burst,
                at: now,
                per_second: limit.per_second,
                burst: limit.burst,
            });
            bucket.refill(now);
            let wait = bucket.wait();

            if wait == 0.0 {
                bucket.tokens -= 1.0;
                continue;
            }

            match limit.action {
                Action::Log => counted.push((Arc::clone(&limit.name), "logged")),
                Action::Delay if Duration::from_secs_f64(wait) <= limit.max_delay => {
                    // The token is taken now and the request waits for it,
                    // so those behind it wait longer.

                    bucket.tokens -= 1.0;
                    delay = delay.max(Duration::from_secs_f64(wait));
                    counted.push((Arc::clone(&limit.name), "delayed"));
                }
                Action::Delay | Action::Reject => {
                    counted.push((Arc::clone(&limit.name), "rejected"));

                    return Outcome {
                        verdict: Verdict::Reject {
                            limit: Arc::clone(limit),
                            retry_after: wait.ceil().max(1.0) as u64,
                        },
                        counted,
                    };
                }
            }
        }

        Outcome {
            verdict: if delay.is_zero() {
                Verdict::Allow
            } else {
                Verdict::Delay(delay)
            },
            counted,
        }
    }
}

/// A route's method restrictions.
#[derive(Debug)]
pub struct Methods {
    deny: Vec<http::Method>,
    not_allowed: bool,
}

/// The methods a 405's Allow field may name.
const KNOWN_METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

impl Methods {
    /// The restrictions a snapshot's route carries; none without any.
    pub fn from_message(message: &pb::Methods) -> Option<Arc<Self>> {
        let deny: Vec<http::Method> = message
            .deny
            .iter()
            .filter_map(|name| http::Method::from_bytes(name.as_bytes()).ok())
            .collect();

        (!deny.is_empty()).then(|| {
            Arc::new(Self {
                deny,
                not_allowed: message.not_allowed,
            })
        })
    }

    /// The answer to a request whose method the route refuses: 503
    /// read_only, or 405 with the known methods left in Allow.
    pub fn refuse(&self, method: &http::Method) -> Option<Problem> {
        if !self.deny.contains(method) {
            return None;
        }

        if !self.not_allowed {
            return Some(Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "read_only",
                "The site is read-only for now, and this request would change it.",
            ));
        }

        let allowed: Vec<&str> = KNOWN_METHODS
            .into_iter()
            .filter(|name| !self.deny.iter().any(|denied| denied.as_str() == *name))
            .collect();
        let mut problem = Problem::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "The route does not take this method.",
        );

        if let Ok(value) = HeaderValue::from_str(&allowed.join(", ")) {
            problem.headers.push((http::header::ALLOW, value));
        }

        Some(problem)
    }
}

/// The 429 for a refused request, with Retry-After and, when the limit
/// says so, the RateLimit-Policy and RateLimit fields.
pub fn rejected(limit: &Limit, retry_after: u64) -> Problem {
    let mut problem = Problem::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "Too many requests: the rate limit was reached.",
    );
    problem.retry_after = Some(u32::try_from(retry_after).unwrap_or(u32::MAX));

    if limit.headers {
        let policy = format!(
            "\"{}\";q={};w={}",
            limit.name,
            limit.burst as u64,
            limit.window()
        );
        let state = format!("\"{}\";r=0;t={retry_after}", limit.name);

        for (name, value) in [("ratelimit-policy", policy), ("ratelimit", state)] {
            if let Ok(value) = HeaderValue::from_str(&value) {
                problem.headers.push((HeaderName::from_static(name), value));
            }
        }
    }

    problem
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(action: &str, key: &[&str], classes: &[&str]) -> Arc<Limit> {
        Limit::from_message(&pb::RateLimit {
            name: "test".to_owned(),
            classes: classes.iter().map(|name| (*name).to_owned()).collect(),
            key: key.iter().map(|part| (*part).to_owned()).collect(),
            per_second: 2.0,
            burst: 3,
            action: action.to_owned(),
            max_delay_ms: 1000,
            ipv4_prefix: 32,
            ipv6_prefix: 64,
            headers: true,
        })
        .unwrap()
    }

    #[test]
    fn a_bucket_allows_its_burst_then_refuses_until_it_fills() {
        let buckets = Buckets::default();
        let limits = [limit("reject", &["address"], &[])];
        let client: IpAddr = "192.0.2.1".parse().unwrap();
        let start = Instant::now();
        let check = |at: Instant| buckets.check(&limits, "default", client, &HeaderMap::new(), at);

        for _ in 0..3 {
            assert!(matches!(check(start).verdict, Verdict::Allow));
        }

        let refused = check(start);
        assert!(matches!(
            refused.verdict,
            Verdict::Reject { retry_after: 1, .. }
        ));
        assert_eq!(refused.counted, vec![(Arc::from("test"), "rejected")]);

        // Half a second at 2/s brings one token back.

        assert!(matches!(
            check(start + Duration::from_millis(500)).verdict,
            Verdict::Allow
        ));

        // Another client has a bucket of its own.

        let other: IpAddr = "192.0.2.2".parse().unwrap();
        let outcome = buckets.check(&limits, "default", other, &HeaderMap::new(), start);
        assert!(matches!(outcome.verdict, Verdict::Allow));
    }

    #[test]
    fn delay_holds_requests_and_log_only_counts() {
        let buckets = Buckets::default();
        let client: IpAddr = "192.0.2.1".parse().unwrap();
        let start = Instant::now();
        let delaying = [limit("delay", &["address"], &[])];

        for _ in 0..3 {
            buckets.check(&delaying, "default", client, &HeaderMap::new(), start);
        }

        match buckets
            .check(&delaying, "default", client, &HeaderMap::new(), start)
            .verdict
        {
            Verdict::Delay(wait) => assert_eq!(wait, Duration::from_millis(500)),
            other => panic!("{other:?}"),
        }

        // The next waits behind it, until the wait is over max_delay.

        assert!(matches!(
            buckets.check(&delaying, "default", client, &HeaderMap::new(), start).verdict,
            Verdict::Delay(wait) if wait == Duration::from_secs(1)
        ));
        assert!(matches!(
            buckets
                .check(&delaying, "default", client, &HeaderMap::new(), start)
                .verdict,
            Verdict::Reject { .. }
        ));

        let logging = [limit("log", &["address"], &[])];
        let other: IpAddr = "192.0.2.9".parse().unwrap();

        for _ in 0..3 {
            buckets.check(&logging, "default", other, &HeaderMap::new(), start);
        }

        let logged = buckets.check(&logging, "default", other, &HeaderMap::new(), start);
        assert!(matches!(logged.verdict, Verdict::Allow));
        assert_eq!(logged.counted, vec![(Arc::from("test"), "logged")]);
    }

    #[test]
    fn a_limit_applies_to_its_classes_and_keys_by_its_parts() {
        let buckets = Buckets::default();
        let limits = [limit(
            "reject",
            &["client_class", "header:x-key"],
            &["ai_crawler"],
        )];
        let start = Instant::now();
        let mut headers = HeaderMap::new();
        headers.insert("x-key", "one".parse().unwrap());

        // Other classes never draw.

        for _ in 0..10 {
            let outcome = buckets.check(
                &limits,
                "default",
                "192.0.2.1".parse().unwrap(),
                &headers,
                start,
            );
            assert!(matches!(outcome.verdict, Verdict::Allow));
        }

        // The class and header make the key, whatever the address.

        for address in ["192.0.2.1", "192.0.2.2", "192.0.2.3"] {
            let outcome = buckets.check(
                &limits,
                "ai_crawler",
                address.parse().unwrap(),
                &headers,
                start,
            );
            assert!(matches!(outcome.verdict, Verdict::Allow));
        }

        let outcome = buckets.check(
            &limits,
            "ai_crawler",
            "192.0.2.4".parse().unwrap(),
            &headers,
            start,
        );
        assert!(matches!(outcome.verdict, Verdict::Reject { .. }));
    }

    #[test]
    fn addresses_are_keyed_by_their_prefix() {
        assert_eq!(
            masked("192.0.2.77".parse().unwrap(), 24, 64),
            "192.0.2.0/24"
        );
        assert_eq!(
            masked("2001:db8:1:2:3:4:5:6".parse().unwrap(), 32, 64),
            "2001:db8:1:2::/64"
        );
        assert_eq!(
            masked("::ffff:192.0.2.1".parse().unwrap(), 32, 64),
            "192.0.2.1/32"
        );
    }

    #[test]
    fn a_rejection_carries_retry_after_and_the_ratelimit_fields() {
        let problem = rejected(&limit("reject", &["address"], &[]), 2);
        assert_eq!(problem.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(problem.retry_after, Some(2));
        let fields: Vec<String> = problem
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {}", value.to_str().unwrap()))
            .collect();
        assert_eq!(
            fields,
            vec![
                "ratelimit-policy: \"test\";q=3;w=2".to_owned(),
                "ratelimit: \"test\";r=0;t=2".to_owned(),
            ]
        );
    }
}
