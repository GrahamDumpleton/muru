//! Connection limits (design specification section 4.18): how many
//! connections a listener holds open, how many one client may hold, and
//! what happens at the limit. Idle connections are not yet evicted; a
//! connection beyond the limit is answered 503 while the overload reserve
//! lasts and closed beyond it, and one over its client's share is closed
//! at once.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::metrics::Sample;

/// The open connections of one listener and what it refused.
pub struct Connections {
    listener: String,
    max: usize,
    per_client: usize,
    reserve: usize,
    open: AtomicUsize,
    overloading: AtomicUsize,
    clients: Mutex<HashMap<IpAddr, usize>>,
    refused_client: AtomicU64,
    overloaded: AtomicU64,
    paused: AtomicU64,
}

/// What happens to a connection just accepted.
pub enum Admission {
    /// Served; the guard releases its place when the connection ends.
    Accept(Guard),
    /// The client holds its share already: closed at once.
    ClientLimit,
    /// The listener is full: answered 503 and closed.
    Overloaded,
    /// The listener is full and the overload reserve too: closed.
    Paused,
}

/// A served connection's place in the counts, given back on drop.
pub struct Guard {
    connections: std::sync::Arc<Connections>,
    client: IpAddr,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.connections.open.fetch_sub(1, Ordering::Relaxed);
        let mut clients = self.connections.clients.lock().expect("clients poisoned");

        if let Some(count) = clients.get_mut(&self.client) {
            *count -= 1;

            if *count == 0 {
                clients.remove(&self.client);
            }
        }
    }
}

impl Connections {
    /// The counts for one listener, with the limits of section 4.18.
    pub fn new(listener: String, max: usize, per_client: usize) -> Self {
        Self {
            listener,
            max: max.max(1),
            per_client,
            reserve: (max / 100).max(1),
            open: AtomicUsize::new(0),
            overloading: AtomicUsize::new(0),
            clients: Mutex::new(HashMap::new()),
            refused_client: AtomicU64::new(0),
            overloaded: AtomicU64::new(0),
            paused: AtomicU64::new(0),
        }
    }

    /// Decide about a connection from `peer`. A trusted proxy has no
    /// per-client limit.
    pub fn admit(self: &std::sync::Arc<Self>, peer: IpAddr, trusted: bool) -> Admission {
        let client = client_key(peer);

        if self.open.load(Ordering::Relaxed) >= self.max {
            // Up to the reserve, a fast 503 says why; beyond it the
            // connection is closed, which is what pausing accept amounts
            // to for a connection already accepted.

            if self.overloading.load(Ordering::Relaxed) < self.reserve {
                self.overloaded.fetch_add(1, Ordering::Relaxed);
                return Admission::Overloaded;
            }

            self.paused.fetch_add(1, Ordering::Relaxed);
            return Admission::Paused;
        }

        let mut clients = self.clients.lock().expect("clients poisoned");
        let held = clients.entry(client).or_insert(0);

        if !trusted && self.per_client > 0 && *held >= self.per_client {
            if *held == 0 {
                clients.remove(&client);
            }

            self.refused_client.fetch_add(1, Ordering::Relaxed);
            return Admission::ClientLimit;
        }

        *held += 1;
        drop(clients);
        self.open.fetch_add(1, Ordering::Relaxed);
        Admission::Accept(Guard {
            connections: std::sync::Arc::clone(self),
            client,
        })
    }

    /// Note that an overload answer is being written, and when it is done.
    pub fn overload_started(&self) {
        self.overloading.fetch_add(1, Ordering::Relaxed);
    }

    /// The counterpart of `overload_started`.
    pub fn overload_finished(&self) {
        self.overloading.fetch_sub(1, Ordering::Relaxed);
    }

    /// Connections open now.
    pub fn open(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }

    /// The listener's samples (section 11.1).
    pub fn samples(&self) -> Vec<Sample> {
        let listener = self.listener.as_str();
        vec![
            Sample::new(
                "muru_connections_open",
                &[("listener", listener)],
                self.open() as f64,
            ),
            Sample::new(
                "muru_connections_refused_total",
                &[("listener", listener), ("reason", "client_limit")],
                self.refused_client.load(Ordering::Relaxed) as f64,
            ),
            Sample::new(
                "muru_connections_refused_total",
                &[("listener", listener), ("reason", "overloaded")],
                self.overloaded.load(Ordering::Relaxed) as f64,
            ),
            Sample::new(
                "muru_connections_refused_total",
                &[("listener", listener), ("reason", "paused")],
                self.paused.load(Ordering::Relaxed) as f64,
            ),
        ]
    }
}

/// The address a client's connections are counted under: the address
/// itself for IPv4, the /64 for IPv6, since one host commonly has a
/// whole /64.
fn client_key(peer: IpAddr) -> IpAddr {
    match peer {
        IpAddr::V4(_) => peer,
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (u128::MAX << 64))),
    }
}

/// The canned answer for a connection the listener cannot take: a 503
/// with Retry-After, written before the connection is closed.
pub const OVERLOADED: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Length: 60\r\n\
Retry-After: 1\r\n\
Connection: close\r\n\
Server: muru\r\n\
\r\n\
503 Service Unavailable\nreason: edge_overloaded\n\nTry again.\n";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn clients_are_held_to_their_share_and_the_listener_to_its_limit() {
        let connections = Arc::new(Connections::new("127.0.0.1:1".to_owned(), 3, 2));
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let first = connections.admit(a, false);
        let second = connections.admit(a, false);
        assert!(matches!(first, Admission::Accept(_)));
        assert!(matches!(second, Admission::Accept(_)));
        assert!(matches!(
            connections.admit(a, false),
            Admission::ClientLimit
        ));
        let proxy = connections.admit(a, true);
        assert!(
            matches!(proxy, Admission::Accept(_)),
            "a trusted proxy has no share"
        );
        assert_eq!(connections.open(), 3);
        assert!(matches!(connections.admit(b, false), Admission::Overloaded));
        connections.overload_started();
        assert!(matches!(connections.admit(b, false), Admission::Paused));
        connections.overload_finished();
        drop(first);
        assert_eq!(connections.open(), 2);
        let late = connections.admit(b, false);
        assert!(matches!(late, Admission::Accept(_)));
        assert_eq!(connections.open(), 3);
        drop(second);
        drop(proxy);
        drop(late);
        assert_eq!(connections.open(), 0);
    }

    #[test]
    fn ipv6_clients_are_grouped_by_their_64() {
        let one: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let two: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(client_key(one), client_key(two));
        assert_ne!(client_key(one), client_key(other));
    }

    #[test]
    fn the_overload_answer_declares_its_own_length() {
        let text = std::str::from_utf8(OVERLOADED).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(
            head.contains(&format!("Content-Length: {}", body.len())),
            "{head}"
        );
    }
}
