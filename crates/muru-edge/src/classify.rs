//! Client classification (design specification section 4.14). Each
//! request gets a class from the first rule whose inputs all match, or
//! `default`: user agents, client addresses, TLS fingerprints, headers and cookies, with a
//! claimed crawler confirmed by reverse and forward DNS. Verification
//! never holds a request: a lookup starts in the background, and until it
//! has confirmed the address the rule does not match, so the request
//! falls through to later rules.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use http::HeaderMap;
use http::header::{HeaderName, USER_AGENT};
use muru_proto::edge::v1 as pb;
use regex::Regex;

use crate::proxy::{Network, parse_network};
use crate::routing::cookie;

/// The class of a request no rule matches.
pub static DEFAULT_CLASS: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("default"));

/// How long a confirmed name is trusted.
const CONFIRMED_FOR: Duration = Duration::from_secs(24 * 3600);

/// How long a failed verification is remembered.
const REFUSED_FOR: Duration = Duration::from_secs(3600);

/// How long each lookup may take.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// The most addresses whose verification is remembered.
const CACHE_LIMIT: usize = 100_000;

/// The rules, in order.
#[derive(Debug, Default)]
pub struct Classifier {
    rules: Vec<Rule>,
}

#[derive(Debug)]
struct Rule {
    name: Arc<str>,
    user_agents: Vec<String>,
    user_agent_regexes: Vec<Regex>,
    addresses: Vec<Network>,
    ja4: Vec<String>,
    headers: Vec<(HeaderName, Regex)>,
    cookies: Vec<(String, Regex)>,
    verify_domains: Vec<String>,
    empty_list: bool,
}

impl Classifier {
    /// The rules a snapshot carries.
    pub fn from_messages(messages: &[pb::ClientClass]) -> Result<Self, String> {
        let mut rules = Vec::new();

        for message in messages {
            let regex = |pattern: &String| {
                Regex::new(pattern)
                    .map_err(|error| format!("client_class {}: {pattern:?}: {error}", message.name))
            };
            let mut addresses = Vec::new();

            for address in &message.addresses {
                addresses.push(
                    parse_network(address)
                        .map_err(|error| format!("client_class {}: {error}", message.name))?,
                );
            }

            let mut headers = Vec::new();

            for (name, pattern) in &message.headers {
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    format!("client_class {}: {name:?} is not a header", message.name)
                })?;
                headers.push((name, regex(pattern)?));
            }

            rules.push(Rule {
                name: Arc::from(message.name.as_str()),
                user_agents: message
                    .user_agents
                    .iter()
                    .map(|agent| agent.to_ascii_lowercase())
                    .collect(),
                user_agent_regexes: message
                    .user_agent_regexes
                    .iter()
                    .map(regex)
                    .collect::<Result<_, _>>()?,
                addresses,
                ja4: message.ja4.clone(),
                empty_list: message.empty_list,
                headers,
                cookies: message
                    .cookies
                    .iter()
                    .map(|(name, pattern)| Ok((name.clone(), regex(pattern)?)))
                    .collect::<Result<_, String>>()?,
                verify_domains: message
                    .verify_domains
                    .iter()
                    .map(|domain| domain.trim_matches('.').to_ascii_lowercase())
                    .collect(),
            });
        }

        Ok(Self { rules })
    }

    /// The class of a request from `client` with `headers`, over a TLS
    /// connection with the fingerprint `ja4` when it has one.
    pub fn classify(
        &self,
        client: IpAddr,
        headers: &HeaderMap,
        ja4: Option<&str>,
        verifier: &Arc<Verifier>,
    ) -> Arc<str> {
        let client = client.to_canonical();

        self.rules
            .iter()
            .find(|rule| rule.matches(client, headers, ja4, verifier))
            .map_or_else(|| Arc::clone(&DEFAULT_CLASS), |rule| Arc::clone(&rule.name))
    }
}

impl Rule {
    fn matches(
        &self,
        client: IpAddr,
        headers: &HeaderMap,
        ja4: Option<&str>,
        verifier: &Arc<Verifier>,
    ) -> bool {
        if self.empty_list {
            return false;
        }

        if !self.user_agents.is_empty() || !self.user_agent_regexes.is_empty() {
            let agent = headers
                .get(USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            let lower = agent.to_ascii_lowercase();
            let matched = self
                .user_agents
                .iter()
                .any(|part| lower.contains(part.as_str()))
                || self
                    .user_agent_regexes
                    .iter()
                    .any(|pattern| pattern.is_match(agent));

            if !matched {
                return false;
            }
        }

        if !self.addresses.is_empty()
            && !self
                .addresses
                .iter()
                .any(|network| network.contains(client))
        {
            return false;
        }

        // An entry ending in * matches the fingerprints it begins.

        if !self.ja4.is_empty()
            && !ja4.is_some_and(|ja4| {
                self.ja4.iter().any(|entry| match entry.strip_suffix('*') {
                    Some(prefix) => ja4.starts_with(prefix),
                    None => ja4 == entry,
                })
            })
        {
            return false;
        }

        for (name, pattern) in &self.headers {
            let value = headers.get(name).and_then(|value| value.to_str().ok());

            if !value.is_some_and(|value| pattern.is_match(value)) {
                return false;
            }
        }

        for (name, pattern) in &self.cookies {
            if !cookie(headers, name).is_some_and(|value| pattern.is_match(&value)) {
                return false;
            }
        }

        // Only a rule that matched on everything else asks for a lookup.

        if !self.verify_domains.is_empty() {
            return verifier.confirmed(client).is_some_and(|name| {
                self.verify_domains
                    .iter()
                    .any(|domain| name == *domain || name.ends_with(&format!(".{domain}")))
            });
        }

        true
    }
}

/// Confirmed names by address, from reverse then forward DNS (section
/// 4.14), shared by every snapshot an edge has.
#[derive(Debug, Default)]
pub struct Verifier {
    cache: Mutex<HashMap<IpAddr, Verification>>,
}

#[derive(Debug, Clone)]
enum Verification {
    Pending,
    Done {
        name: Option<String>,
        until: Instant,
    },
}

impl Verifier {
    /// An empty cache.
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// The name `address` is confirmed to have; none while a lookup is
    /// pending, when it found none, or before the first, which this
    /// starts.
    pub fn confirmed(self: &Arc<Self>, address: IpAddr) -> Option<String> {
        let now = Instant::now();
        let mut cache = self.cache.lock().expect("verifier poisoned");

        match cache.get(&address) {
            Some(Verification::Pending) => return None,
            Some(Verification::Done { name, until }) if now < *until => return name.clone(),
            _ => {}
        }

        if cache.len() >= CACHE_LIMIT {
            cache.retain(
                |_, entry| matches!(entry, Verification::Done { until, .. } if now < *until),
            );
        }

        cache.insert(address, Verification::Pending);
        drop(cache);

        // A lookup needs a runtime; without one, as in a plain unit test,
        // the address stays unconfirmed.

        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let verifier = Arc::clone(self);

            runtime.spawn(async move {
                let name = lookup(address).await;
                verifier.record(address, name);
            });
        }

        None
    }

    fn record(&self, address: IpAddr, name: Option<String>) {
        let until = Instant::now()
            + if name.is_some() {
                CONFIRMED_FOR
            } else {
                REFUSED_FOR
            };
        let mut cache = self.cache.lock().expect("verifier poisoned");
        cache.insert(address, Verification::Done { name, until });
    }
}

/// The name `address` reverse-resolves to, when that name forward-resolves
/// to the address again.
async fn lookup(address: IpAddr) -> Option<String> {
    let reverse = tokio::task::spawn_blocking(move || reverse_name(address));
    let name = tokio::time::timeout(LOOKUP_TIMEOUT, reverse)
        .await
        .ok()?
        .ok()??;
    let forward = tokio::net::lookup_host((name.as_str(), 0));
    let found = tokio::time::timeout(LOOKUP_TIMEOUT, forward)
        .await
        .ok()?
        .ok()?;
    let confirmed = found
        .map(|socket| socket.ip().to_canonical())
        .any(|ip| ip == address);
    confirmed.then_some(name)
}

/// The PTR name of `address`, through the system resolver.
fn reverse_name(address: IpAddr) -> Option<String> {
    let mut host = [0 as libc::c_char; 1025];

    let result = match address {
        IpAddr::V4(ip) => {
            let socket = socket_v4(ip);

            // SAFETY: the address structure and the output buffer live
            // for the call, and the lengths given are theirs.

            unsafe {
                libc::getnameinfo(
                    (&raw const socket).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    host.as_mut_ptr(),
                    host.len() as libc::socklen_t,
                    std::ptr::null_mut(),
                    0,
                    libc::NI_NAMEREQD,
                )
            }
        }
        IpAddr::V6(ip) => {
            let socket = socket_v6(ip);

            // SAFETY: as above, for the IPv6 structure.

            unsafe {
                libc::getnameinfo(
                    (&raw const socket).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    host.as_mut_ptr(),
                    host.len() as libc::socklen_t,
                    std::ptr::null_mut(),
                    0,
                    libc::NI_NAMEREQD,
                )
            }
        }
    };

    if result != 0 {
        return None;
    }

    // SAFETY: getnameinfo succeeded, so the buffer holds a terminated
    // string within its length.

    let name = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) };
    name.to_str()
        .ok()
        .map(|name| name.trim_end_matches('.').to_ascii_lowercase())
}

fn socket_v4(ip: Ipv4Addr) -> libc::sockaddr_in {
    // SAFETY: an all-zero sockaddr_in is valid; the fields that matter
    // are set below.

    let mut socket: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    socket.sin_family = libc::AF_INET as libc::sa_family_t;
    socket.sin_addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(ip.octets()),
    };

    #[cfg(target_os = "macos")]
    {
        socket.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    }

    socket
}

fn socket_v6(ip: Ipv6Addr) -> libc::sockaddr_in6 {
    // SAFETY: as for socket_v4.

    let mut socket: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    socket.sin6_family = libc::AF_INET6 as libc::sa_family_t;
    socket.sin6_addr = libc::in6_addr {
        s6_addr: ip.octets(),
    };

    #[cfg(target_os = "macos")]
    {
        socket.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
    }

    socket
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classifier(classes: Vec<pb::ClientClass>) -> Classifier {
        Classifier::from_messages(&classes).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();

        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }

        map
    }

    #[test]
    fn the_first_rule_that_matches_wins_and_default_is_the_rest() {
        let rules = classifier(vec![
            pb::ClientClass {
                name: "partner".to_owned(),
                addresses: vec!["203.0.113.0/24".to_owned()],
                ..Default::default()
            },
            pb::ClientClass {
                name: "ai_crawler".to_owned(),
                user_agents: vec!["GPTBot".to_owned()],
                user_agent_regexes: vec!["^Claude".to_owned()],
                ..Default::default()
            },
            pb::ClientClass {
                name: "scored".to_owned(),
                headers: [("x-bot-score".to_owned(), "^[0-2]?[0-9]$".to_owned())].into(),
                cookies: [("tier".to_owned(), "^free$".to_owned())].into(),
                ..Default::default()
            },
        ]);
        let verifier = Verifier::new();
        let outside: IpAddr = "198.51.100.7".parse().unwrap();

        let class = |client: IpAddr, pairs: &[(&str, &str)]| {
            rules
                .classify(client, &headers(pairs), None, &verifier)
                .to_string()
        };

        assert_eq!(
            class("203.0.113.9".parse().unwrap(), &[("user-agent", "gptbot")]),
            "partner"
        );
        assert_eq!(
            class(outside, &[("user-agent", "Mozilla gptbot/1.0")]),
            "ai_crawler"
        );
        assert_eq!(class(outside, &[("user-agent", "ClaudeBot")]), "ai_crawler");
        assert_eq!(class(outside, &[("user-agent", "a ClaudeBot")]), "default");
        assert_eq!(
            class(
                outside,
                &[("x-bot-score", "12"), ("cookie", "a=1; tier=free")]
            ),
            "scored"
        );
        assert_eq!(class(outside, &[("x-bot-score", "12")]), "default");
        assert_eq!(
            class(outside, &[("x-bot-score", "99"), ("cookie", "tier=free")]),
            "default"
        );
        assert_eq!(class(outside, &[]), "default");
    }

    #[test]
    fn a_fingerprint_matches_exactly_or_by_its_beginning() {
        let rules = classifier(vec![
            pb::ClientClass {
                name: "scripted".to_owned(),
                ja4: vec!["t13d1516h2_8daaf6152771_b186095e22b6".to_owned()],
                ..Default::default()
            },
            pb::ClientClass {
                name: "old_tls".to_owned(),
                ja4: vec!["t12*".to_owned()],
                ..Default::default()
            },
        ]);
        let verifier = Verifier::new();
        let client: IpAddr = "198.51.100.7".parse().unwrap();
        let class = |ja4: Option<&str>| {
            rules
                .classify(client, &HeaderMap::new(), ja4, &verifier)
                .to_string()
        };

        assert_eq!(
            class(Some("t13d1516h2_8daaf6152771_b186095e22b6")),
            "scripted"
        );
        assert_eq!(
            class(Some("t12d1312h2_8daaf6152771_b186095e22b6")),
            "old_tls"
        );
        assert_eq!(
            class(Some("t13d1516h2_8daaf6152771_000000000000")),
            "default"
        );
        assert_eq!(class(None), "default");
    }

    #[tokio::test]
    async fn a_claimed_name_falls_through_until_dns_confirms_it() {
        let rules = classifier(vec![
            pb::ClientClass {
                name: "verified".to_owned(),
                user_agents: vec!["Testbot".to_owned()],
                verify_domains: vec!["localhost".to_owned()],
                ..Default::default()
            },
            pb::ClientClass {
                name: "claimed".to_owned(),
                user_agents: vec!["Testbot".to_owned()],
                ..Default::default()
            },
        ]);
        let verifier = Verifier::new();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let request = headers(&[("user-agent", "Testbot/1.0")]);

        assert_eq!(
            &*rules.classify(loopback, &request, None, &verifier),
            "claimed"
        );

        let deadline = Instant::now() + Duration::from_secs(10);

        while Instant::now() < deadline
            && matches!(
                verifier.cache.lock().unwrap().get(&loopback),
                Some(Verification::Pending)
            )
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            &*rules.classify(loopback, &request, None, &verifier),
            "verified"
        );

        // An address whose name is not in the domains never is.

        verifier.record(
            "192.0.2.1".parse().unwrap(),
            Some("crawl.example.org".to_owned()),
        );
        assert_eq!(
            &*rules.classify("192.0.2.1".parse().unwrap(), &request, None, &verifier),
            "claimed"
        );
    }
}
