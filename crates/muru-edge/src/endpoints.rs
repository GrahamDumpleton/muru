//! Open WebSockets counted by endpoint, for the dashboard and the CLI.
//!
//! Paths often carry identifiers (`/ws/chat/8c1f2a…`), so counting by
//! path would give one row per socket. A path is first normalised, with
//! segments that look like identifiers (numbers, UUIDs, ULIDs, long hex
//! or long tokens of letters and digits) named `{id}`, and the paths are
//! then put in a tree of segments where a segment with more distinct
//! values under one parent than `FAN_OUT` is counted as one, `*`. A
//! shared prefix is kept only as far as the paths agree, so `/ws/chat/…`
//! and `/ws/notify/…` stay apart while their ids fold together.
//!
//! This is done over the sockets open at the moment it is asked for, so
//! the number of rows is bounded by them; the metrics never carry paths.

use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;

use muru_proto::edge::v1 as pb;

use crate::access::AccessLog;

/// Distinct values a segment may have under one parent before they are
/// counted together as `*`.
pub const FAN_OUT: usize = 20;

/// Count paths by endpoint: each pattern with how many paths it covers,
/// the most first, then by pattern.
pub fn endpoints<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<(String, usize)> {
    let mut root = Node::default();

    for path in paths {
        let mut node = &mut root;

        for segment in path.trim_start_matches('/').split('/') {
            node = node.children.entry(normalise(segment)).or_default();
        }

        node.ends += 1;
    }

    root.collapse();
    let mut found = Vec::new();
    root.flatten(String::new(), &mut found);
    found.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    found
}

/// The WebSockets open on an edge, of one application or all: counted by
/// endpoint for each application, and the first `limit`, the longest
/// open first.
pub fn report(log: &AccessLog, app: &str, limit: usize) -> pb::WebSockets {
    let open: Vec<_> = log
        .websockets()
        .into_iter()
        .filter(|socket| app.is_empty() || socket.app == app)
        .collect();
    let mut by_app: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    for socket in &open {
        by_app
            .entry(socket.app.as_str())
            .or_default()
            .push(socket.path.as_str());
    }

    let endpoints = by_app
        .into_iter()
        .flat_map(|(app, paths)| {
            endpoints(paths)
                .into_iter()
                .map(move |(pattern, count)| pb::WebSocketEndpoint {
                    app: app.to_owned(),
                    pattern,
                    open: u32::try_from(count).unwrap_or(u32::MAX),
                })
        })
        .collect();
    let sockets = open
        .iter()
        .take(limit)
        .map(|socket| pb::OpenWebSocket {
            request_id: socket.request_id.clone(),
            client: socket.client.to_string(),
            app: socket.app.clone(),
            release: socket.release.clone(),
            host: socket.host.clone(),
            group: socket.group.clone(),
            mount: socket.mount.clone(),
            path: socket.path.clone(),
            worker: socket
                .worker
                .map_or(-1, |worker| i32::try_from(worker).unwrap_or(i32::MAX)),
            opened_unix_ms: socket
                .opened
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64),
            received: socket.received,
            sent: socket.sent,
            received_bytes: socket.received_bytes,
            sent_bytes: socket.sent_bytes,
        })
        .collect();

    pb::WebSockets {
        open: u32::try_from(open.len()).unwrap_or(u32::MAX),
        endpoints,
        sockets,
    }
}

/// One segment's place in the tree: the paths that end here, and the
/// segments that follow.
#[derive(Debug, Default)]
struct Node {
    ends: usize,
    children: BTreeMap<String, Node>,
}

impl Node {
    /// Fold children past the fan-out into `*`, from the top down, so a
    /// fold below sees the merged subtrees.
    fn collapse(&mut self) {
        if self.children.len() > FAN_OUT {
            let mut merged = Node::default();

            for (_, child) in std::mem::take(&mut self.children) {
                merged.merge(child);
            }

            self.children.insert("*".to_owned(), merged);
        }

        for child in self.children.values_mut() {
            child.collapse();
        }
    }

    fn merge(&mut self, other: Node) {
        self.ends += other.ends;

        for (segment, child) in other.children {
            self.children.entry(segment).or_default().merge(child);
        }
    }

    fn flatten(&self, prefix: String, into: &mut Vec<(String, usize)>) {
        if self.ends > 0 {
            into.push((
                if prefix.is_empty() {
                    "/".to_owned()
                } else {
                    prefix.clone()
                },
                self.ends,
            ));
        }

        for (segment, child) in &self.children {
            child.flatten(format!("{prefix}/{segment}"), into);
        }
    }
}

/// A segment as counted: `{id}` when it looks like an identifier.
fn normalise(segment: &str) -> String {
    let digits = segment.bytes().filter(u8::is_ascii_digit).count();
    let letters = segment.bytes().filter(u8::is_ascii_alphabetic).count();
    let token = segment
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    let hex = segment.bytes().all(|byte| byte.is_ascii_hexdigit());
    let uuid = segment.len() == 36
        && segment.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    let identifier = (!segment.is_empty() && digits == segment.len())
        || uuid
        || (segment.len() >= 16 && hex)
        || (segment.len() >= 20 && token && digits > 0 && letters > 0);

    if identifier {
        "{id}".to_owned()
    } else {
        segment.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_named() {
        assert_eq!(normalise("42"), "{id}");
        assert_eq!(normalise("8c1f2a3e-1b2c-4d5e-8f90-a1b2c3d4e5f6"), "{id}");
        assert_eq!(normalise("01J9ZB4X7Q8R2S3T4V5W6X7Y8Z"), "{id}");
        assert_eq!(normalise("deadbeefdeadbeef"), "{id}");
        assert_eq!(normalise("chat"), "chat");
        assert_eq!(normalise("v2"), "v2");
        assert_eq!(normalise(""), "");
    }

    #[test]
    fn ids_fold_together_and_endpoints_stay_apart() {
        let paths = [
            "/ws/chat/1",
            "/ws/chat/2",
            "/ws/chat/3",
            "/ws/notify/8c1f2a3e-1b2c-4d5e-8f90-a1b2c3d4e5f6",
            "/ws",
        ];
        assert_eq!(
            endpoints(paths),
            vec![
                ("/ws/chat/{id}".to_owned(), 3),
                ("/ws".to_owned(), 1),
                ("/ws/notify/{id}".to_owned(), 1),
            ]
        );
    }

    #[test]
    fn many_distinct_names_fold_into_a_wildcard() {
        let rooms: Vec<String> = (0..30)
            .map(|room| format!("/ws/room-{room}x/feed"))
            .collect();
        let mut paths: Vec<&str> = rooms.iter().map(String::as_str).collect();
        paths.push("/ws/lobby");
        let counted = endpoints(paths);
        assert_eq!(
            counted,
            vec![("/ws/*/feed".to_owned(), 30), ("/ws/*".to_owned(), 1)]
        );
    }

    #[test]
    fn a_few_distinct_names_are_kept() {
        let counted = endpoints(["/ws/alpha", "/ws/beta", "/ws/beta"]);
        assert_eq!(
            counted,
            vec![("/ws/beta".to_owned(), 2), ("/ws/alpha".to_owned(), 1)]
        );
    }

    #[test]
    fn the_root_is_a_slash() {
        assert_eq!(endpoints(["/"]), vec![("/".to_owned(), 1)]);
    }
}
