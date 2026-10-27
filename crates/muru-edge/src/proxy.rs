//! Trusted proxies and forwarding headers (design specification section
//! 4.7). Nothing is trusted by default: a forwarding header from an
//! untrusted peer is removed before it can reach an application. From a
//! trusted peer, only the configured header types are applied, every
//! equivalent is removed, and the resolved client, scheme, host, port and
//! prefix travel to the worker as typed fields, with the original peer
//! beside them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use http::HeaderMap;
use muru_proto::edge::v1 as pb;

/// Every forwarding header the edge knows, removed from untrusted peers.
const FORWARDING_HEADERS: [&str; 7] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-forwarded-prefix",
    "x-real-ip",
];

/// Who is trusted: an address range, or a class of addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Network {
    Range(IpAddr, u8),
    Loopback,
    Private,
    Unix,
}

impl Network {
    pub(crate) fn contains(&self, address: IpAddr) -> bool {
        match self {
            Network::Range(network, prefix) => in_range(*network, *prefix, address),
            Network::Loopback => address.is_loopback(),
            Network::Private => is_private(address),
            Network::Unix => false,
        }
    }
}

/// Whether `address` lies in the `prefix`-bit network of `network`.
fn in_range(network: IpAddr, prefix: u8, address: IpAddr) -> bool {
    match (network, address) {
        (IpAddr::V4(network), IpAddr::V4(address)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix.min(32)))
            };
            u32::from(network) & mask == u32::from(address) & mask
        }
        (IpAddr::V6(network), IpAddr::V6(address)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix.min(128)))
            };
            u128::from(network) & mask == u128::from(address) & mask
        }
        (IpAddr::V4(network), IpAddr::V6(address)) => match address.to_ipv4_mapped() {
            Some(mapped) => in_range(IpAddr::V4(network), prefix, IpAddr::V4(mapped)),
            None => false,
        },
        (IpAddr::V6(_), IpAddr::V4(_)) => false,
    }
}

/// RFC 1918 and RFC 4193 addresses, and link-local ones.
fn is_private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private() || address.is_link_local(),
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(mapped) => mapped.is_private() || mapped.is_link_local(),
            None => address.is_unique_local() || address.is_unicast_link_local(),
        },
    }
}

/// Parse an entry of `trusted`: an address, a CIDR range, or a word.
pub(crate) fn parse_network(text: &str) -> Result<Network, String> {
    let text = text.trim();

    match text {
        "loopback" => return Ok(Network::Loopback),
        "private" => return Ok(Network::Private),
        "unix" => return Ok(Network::Unix),
        _ => {}
    }

    let (address, prefix) = match text.split_once('/') {
        Some((address, prefix)) => {
            let prefix: u8 = prefix
                .parse()
                .map_err(|_| format!("{text:?} is not an address range"))?;
            (address, Some(prefix))
        }
        None => (text, None),
    };

    let address: IpAddr = address
        .parse()
        .map_err(|_| format!("{text:?} is not an address, a range, loopback, private or unix"))?;
    let bits = if address.is_ipv4() { 32 } else { 128 };
    let prefix = prefix.unwrap_or(bits);

    if prefix > bits {
        return Err(format!("{text:?}: the prefix is longer than the address"));
    }

    Ok(Network::Range(address, prefix))
}

/// A list of networks, as `[proxy]` trusted takes them, for other
/// settings that name clients: a listener's h2c (section 6.13).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Networks(Vec<Network>);

impl Networks {
    /// Parse the entries: addresses, ranges, loopback, private or unix.
    pub fn parse(entries: &[String]) -> Result<Self, String> {
        entries
            .iter()
            .map(|entry| parse_network(entry))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    /// Whether a peer lies in one of them.
    pub fn contains(&self, peer: &SocketAddr) -> bool {
        self.0.iter().any(|network| network.contains(peer.ip()))
    }
}

/// The compiled `[proxy]` table.
#[derive(Debug, Clone, Default)]
pub struct Trust {
    networks: Vec<Network>,
    headers: Vec<String>,
    recursive: bool,
    consume: bool,
}

/// What the forwarding headers of a trusted peer said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resolved {
    /// The client, when a forwarded address was taken.
    pub client: Option<SocketAddr>,
    /// The scheme the client used, when forwarded.
    pub scheme: Option<String>,
    /// The host the client asked for, when forwarded.
    pub host: Option<String>,
    /// The port the client connected to, when forwarded.
    pub port: Option<u16>,
    /// The path prefix the proxy stripped, when forwarded.
    pub prefix: Option<String>,
}

impl Trust {
    /// Compile the snapshot's proxy settings.
    pub fn from_snapshot(proxy: &pb::Proxy) -> Result<Self, String> {
        let mut networks = Vec::new();

        for entry in &proxy.trusted {
            networks.push(parse_network(entry)?);
        }

        let mut headers = Vec::new();

        for header in &proxy.headers {
            let header = header.trim().to_ascii_lowercase();

            if !FORWARDING_HEADERS.contains(&header.as_str()) {
                return Err(format!("{header:?} is not a forwarding header type"));
            }

            headers.push(header);
        }

        Ok(Self {
            networks,
            headers,
            recursive: proxy.recursive,
            consume: proxy.consume,
        })
    }

    /// Whether a peer may speak for the client.
    pub fn trusts(&self, peer: &SocketAddr) -> bool {
        self.networks
            .iter()
            .any(|network| network.contains(peer.ip()))
    }

    /// Apply the headers of a request from `peer`: from an untrusted peer
    /// every forwarding header goes and nothing is resolved; from a
    /// trusted one the configured types are read, then the headers are
    /// kept or consumed as configured.
    pub fn resolve(&self, peer: &SocketAddr, headers: &mut HeaderMap) -> Resolved {
        if !self.trusts(peer) {
            remove_all(headers);
            return Resolved::default();
        }

        // Each type is read in the configured order, and the first that
        // gives a value wins, so the order of `headers` is the precedence.

        let mut resolved = Resolved::default();

        for header in &self.headers {
            match header.as_str() {
                "forwarded" => self.read_forwarded(headers, &mut resolved),
                "x-forwarded-for" => self.read_for(headers, &mut resolved),
                "x-forwarded-proto" => read_last(headers, "x-forwarded-proto", |value| {
                    resolved.scheme = resolved.scheme.take().or_else(|| scheme(value));
                }),
                "x-forwarded-host" => read_last(headers, "x-forwarded-host", |value| {
                    resolved.host = resolved.host.take().or_else(|| host(value));
                }),
                "x-forwarded-port" => read_last(headers, "x-forwarded-port", |value| {
                    resolved.port = resolved.port.or_else(|| value.trim().parse().ok());
                }),
                "x-forwarded-prefix" => read_last(headers, "x-forwarded-prefix", |value| {
                    resolved.prefix = resolved.prefix.take().or_else(|| prefix(value));
                }),
                "x-real-ip" => read_last(headers, "x-real-ip", |value| {
                    resolved.client = resolved.client.or_else(|| address(value));
                }),
                _ => {}
            }
        }

        // Section 4.7: equivalents of what was applied never reach the
        // application; with consume, nothing forwarded does.

        if self.consume {
            remove_all(headers);
        } else {
            for name in FORWARDING_HEADERS {
                if !self.headers.iter().any(|header| header == name) {
                    headers.remove(name);
                }
            }
        }

        resolved
    }

    /// The Forwarded header (RFC 7239): `for`, `proto` and `host` pairs,
    /// one element per proxy, the nearest last.
    fn read_forwarded(&self, headers: &HeaderMap, resolved: &mut Resolved) {
        let mut elements: Vec<(Option<SocketAddr>, Option<String>, Option<String>)> = Vec::new();

        for value in headers.get_all("forwarded") {
            let Ok(text) = value.to_str() else {
                continue;
            };

            for element in text.split(',') {
                let mut client = None;
                let mut proto = None;
                let mut host_name = None;

                for pair in element.split(';') {
                    let Some((name, value)) = pair.split_once('=') else {
                        continue;
                    };
                    let value = value.trim().trim_matches('"');

                    match name.trim().to_ascii_lowercase().as_str() {
                        "for" => client = address(value),
                        "proto" => proto = scheme(value),
                        "host" => host_name = host(value),
                        _ => {}
                    }
                }

                elements.push((client, proto, host_name));
            }
        }

        if resolved.client.is_none() {
            resolved.client = self.choose(elements.iter().map(|element| element.0));
        }

        if let Some((_, proto, host_name)) = elements.last() {
            if resolved.scheme.is_none() {
                resolved.scheme = proto.clone();
            }

            if resolved.host.is_none() {
                resolved.host = host_name.clone();
            }
        }
    }

    /// X-Forwarded-For: addresses left to right from the client.
    fn read_for(&self, headers: &HeaderMap, resolved: &mut Resolved) {
        let mut chain = Vec::new();

        for value in headers.get_all("x-forwarded-for") {
            if let Ok(text) = value.to_str() {
                chain.extend(text.split(',').map(address));
            }
        }

        if resolved.client.is_none() {
            resolved.client = self.choose(chain.into_iter());
        }
    }

    /// The client from a chain of forwarded addresses: the rightmost, or
    /// with recursion the first from the right that is not itself a
    /// trusted proxy (the leftmost when every one is). An address that
    /// does not parse ends the walk, so a malformed value is never taken
    /// as the client.
    fn choose(&self, chain: impl Iterator<Item = Option<SocketAddr>>) -> Option<SocketAddr> {
        let chain: Vec<Option<SocketAddr>> = chain.collect();
        let mut chosen = None;

        for candidate in chain.iter().rev() {
            let Some(candidate) = candidate else {
                break;
            };
            chosen = Some(*candidate);

            if !self.recursive || !self.trusts(candidate) {
                break;
            }
        }

        chosen
    }
}

fn remove_all(headers: &mut HeaderMap) {
    for name in FORWARDING_HEADERS {
        headers.remove(name);
    }
}

/// Read the last value of a header, which is the one the nearest proxy set.
fn read_last(headers: &HeaderMap, name: &'static str, mut take: impl FnMut(&str)) {
    let last = headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .rfind(|value| !value.is_empty());

    if let Some(value) = last {
        take(value);
    }
}

/// An address as forwarding headers write it: bare, bracketed IPv6, with
/// or without a port, or an RFC 7239 obfuscated or unknown identifier,
/// which gives nothing.
fn address(text: &str) -> Option<SocketAddr> {
    let text = text.trim().trim_matches('"');

    if text.is_empty() || text.starts_with('_') || text == "unknown" {
        return None;
    }

    if let Ok(socket) = text.parse::<SocketAddr>() {
        return Some(socket);
    }

    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, 0));
    }

    if let Some(inner) = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return inner
            .parse::<Ipv6Addr>()
            .ok()
            .map(|ip| SocketAddr::new(IpAddr::V6(ip), 0));
    }

    if let Some((host, port)) = text.rsplit_once(':') {
        let ip: Option<IpAddr> = host.parse::<Ipv4Addr>().ok().map(IpAddr::V4);

        if let (Some(ip), Ok(port)) = (ip, port.parse::<u16>()) {
            return Some(SocketAddr::new(ip, port));
        }
    }

    None
}

fn scheme(text: &str) -> Option<String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "http" => Some("http".to_owned()),
        "https" => Some("https".to_owned()),
        _ => None,
    }
}

/// A host value: a name or address with an optional port, no spaces or
/// control characters.
fn host(text: &str) -> Option<String> {
    let text = text.trim();
    let valid = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'/' && byte != b'\\');
    valid.then(|| text.to_owned())
}

/// A prefix as a path: starting with a slash, with no trailing one.
fn prefix(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches('/');
    let valid = text.starts_with('/')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'?' && byte != b'#');
    valid.then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use http::header::HeaderName;

    fn trust(trusted: &[&str], headers: &[&str], recursive: bool, consume: bool) -> Trust {
        Trust::from_snapshot(&pb::Proxy {
            trusted: trusted.iter().map(|text| (*text).to_owned()).collect(),
            headers: headers.iter().map(|text| (*text).to_owned()).collect(),
            recursive,
            consume,
        })
        .unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();

        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }

        map
    }

    fn peer(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn networks_parse_and_match() {
        assert!(
            parse_network("10.0.0.0/8")
                .unwrap()
                .contains(peer("10.9.8.7:1").ip())
        );
        assert!(
            !parse_network("10.0.0.0/8")
                .unwrap()
                .contains(peer("11.0.0.1:1").ip())
        );
        assert!(
            parse_network("192.0.2.1")
                .unwrap()
                .contains(peer("192.0.2.1:5").ip())
        );
        assert!(
            parse_network("loopback")
                .unwrap()
                .contains(peer("[::1]:5").ip())
        );
        assert!(
            parse_network("private")
                .unwrap()
                .contains(peer("172.16.0.9:5").ip())
        );
        assert!(
            parse_network("fd00::/8")
                .unwrap()
                .contains(peer("[fd12::1]:5").ip())
        );
        assert!(
            parse_network("10.0.0.0/8")
                .unwrap()
                .contains(peer("[::ffff:10.1.1.1]:5").ip())
        );
        assert!(parse_network("10.0.0.0/33").is_err());
        assert!(parse_network("somewhere").is_err());
        assert!(
            Trust::from_snapshot(&pb::Proxy {
                headers: vec!["x-forwarded-everything".to_owned()],
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn untrusted_peers_lose_every_forwarding_header() {
        let trust = trust(&["10.0.0.0/8"], &["x-forwarded-for"], true, false);
        let mut map = headers(&[("x-forwarded-for", "203.0.113.5"), ("x-real-ip", "1.2.3.4")]);

        let resolved = trust.resolve(&peer("192.0.2.1:4"), &mut map);
        assert_eq!(resolved, Resolved::default());
        assert!(map.is_empty());
    }

    #[test]
    fn trusted_peers_resolve_the_chain_recursively() {
        let trust = trust(
            &["10.0.0.0/8", "loopback"],
            &["x-forwarded-for", "x-forwarded-proto", "x-forwarded-host"],
            true,
            false,
        );
        let mut map = headers(&[
            ("x-forwarded-for", "203.0.113.5, 10.1.1.1"),
            ("x-forwarded-for", "10.2.2.2"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "shop.example.com"),
            ("x-real-ip", "198.51.100.9"),
        ]);

        let resolved = trust.resolve(&peer("127.0.0.1:4"), &mut map);
        assert_eq!(resolved.client, Some(peer("203.0.113.5:0")));
        assert_eq!(resolved.scheme.as_deref(), Some("https"));
        assert_eq!(resolved.host.as_deref(), Some("shop.example.com"));
        assert!(
            map.contains_key("x-forwarded-for"),
            "configured headers are kept"
        );
        assert!(
            !map.contains_key("x-real-ip"),
            "other forwarding headers go"
        );

        let rightmost = Trust {
            recursive: false,
            ..trust.clone()
        };
        let mut map = headers(&[("x-forwarded-for", "203.0.113.5, 10.1.1.1")]);
        assert_eq!(
            rightmost.resolve(&peer("127.0.0.1:4"), &mut map).client,
            Some(peer("10.1.1.1:0"))
        );

        let consuming = Trust {
            consume: true,
            ..trust
        };
        let mut map = headers(&[("x-forwarded-for", "203.0.113.5")]);
        consuming.resolve(&peer("127.0.0.1:4"), &mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn the_forwarded_header_is_read_as_rfc_7239_writes_it() {
        let trust = trust(&["loopback"], &["forwarded"], true, false);
        let mut map = headers(&[(
            "forwarded",
            "for=\"[2001:db8::1]:4711\";proto=https;host=shop.example.com, for=127.0.0.1",
        )]);

        let resolved = trust.resolve(&peer("127.0.0.1:4"), &mut map);
        assert_eq!(resolved.client, Some(peer("[2001:db8::1]:4711")));
        assert_eq!(resolved.scheme, None, "proto came from a farther element");
        assert_eq!(resolved.host, None);

        let mut map = headers(&[("forwarded", "for=_hidden;proto=https")]);
        let resolved = trust.resolve(&peer("127.0.0.1:4"), &mut map);
        assert_eq!(
            resolved.client, None,
            "an obfuscated identifier gives no address"
        );
        assert_eq!(resolved.scheme.as_deref(), Some("https"));
    }

    #[test]
    fn malformed_values_are_ignored() {
        let trust = trust(
            &["loopback"],
            &[
                "x-forwarded-for",
                "x-forwarded-proto",
                "x-forwarded-port",
                "x-forwarded-prefix",
            ],
            true,
            false,
        );
        let mut map = headers(&[
            ("x-forwarded-for", "not an address"),
            ("x-forwarded-proto", "gopher"),
            ("x-forwarded-port", "lots"),
            ("x-forwarded-prefix", "no-slash"),
        ]);

        let resolved = trust.resolve(&peer("127.0.0.1:4"), &mut map);
        assert_eq!(resolved, Resolved::default());

        let mut map = headers(&[
            ("x-forwarded-prefix", "/app/"),
            ("x-forwarded-port", "8443"),
        ]);
        let resolved = trust.resolve(&peer("127.0.0.1:4"), &mut map);
        assert_eq!(resolved.prefix.as_deref(), Some("/app"));
        assert_eq!(resolved.port, Some(8443));
    }
}
