//! Event sinks (design specification section 11.5): webhooks that receive
//! every event they are filtered to as a signed CloudEvents request. Each
//! sink delivers in order from a cursor the store keeps, retrying until
//! the receiver answers, so an outage or a daemon restart loses nothing
//! and a receiver sees every event at least once.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{CONTENT_TYPE, USER_AGENT};
use http::{Request, Uri};
use http_body_util::Full;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::events::Events;
use crate::model::Event;
use crate::store::Store;

/// The content type of a delivery.
const CONTENT_TYPE_CLOUDEVENTS: &str = "application/cloudevents+json";

/// How long one delivery may take.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest wait between attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Consecutive failures before the outage is reported as an event.
const FAILURES_TO_REPORT: u32 = 3;

/// The levels an event can have, for the `levels` filter.
const LEVELS: [&str; 4] = ["info", "notice", "warning", "error"];

/// A webhook sink, from an `[[events.sink]]` entry.
#[derive(Debug, Clone)]
pub struct Webhook {
    /// The sink's name, which names the cursor and the events about it.
    pub name: String,
    /// Where deliveries go.
    pub url: Uri,
    /// The key the body is signed with.
    pub secret: Vec<u8>,
    /// Event types delivered, as patterns such as `release.*`; every type
    /// when empty.
    pub types: Vec<String>,
    /// Levels delivered; every level when empty.
    pub levels: Vec<String>,
}

impl Webhook {
    /// Check an entry of the configuration file and read its secret.
    pub fn from_config(sink: &muru_config::EventSink) -> Result<Self, String> {
        if !crate::model::valid_name(&sink.name) {
            return Err(format!(
                "{:?} is not a valid sink name: lowercase letters, digits and hyphens",
                sink.name
            ));
        }

        let url: Uri = sink
            .url
            .parse()
            .map_err(|_| format!("sink {}: {:?} is not a URL", sink.name, sink.url))?;

        if !matches!(url.scheme_str(), Some("http" | "https")) || url.host().is_none() {
            return Err(format!(
                "sink {}: the URL must be http:// or https:// with a host",
                sink.name
            ));
        }

        let secret = match (&sink.secret, &sink.secret_file) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "sink {}: give secret or secret_file, not both",
                    sink.name
                ));
            }
            (Some(secret), None) => secret.as_bytes().to_vec(),
            (None, Some(path)) => std::fs::read_to_string(path)
                .map_err(|error| format!("sink {}: reading {path}: {error}", sink.name))?
                .trim()
                .as_bytes()
                .to_vec(),
            (None, None) => {
                return Err(format!(
                    "sink {}: a secret (or secret_file) is needed to sign deliveries",
                    sink.name
                ));
            }
        };

        if secret.is_empty() {
            return Err(format!("sink {}: the secret is empty", sink.name));
        }

        if let Some(level) = sink
            .levels
            .iter()
            .find(|level| !LEVELS.contains(&level.as_str()))
        {
            return Err(format!(
                "sink {}: {level:?} is not a level: expected one of {}",
                sink.name,
                LEVELS.join(", ")
            ));
        }

        Ok(Self {
            name: sink.name.clone(),
            url,
            secret,
            types: sink.types.clone(),
            levels: sink.levels.clone(),
        })
    }

    /// Whether the sink's filters let an event through.
    pub fn accepts(&self, event: &Event) -> bool {
        let type_matches = self.types.is_empty()
            || self
                .types
                .iter()
                .any(|pattern| muru_edge::access::glob_matches(pattern, &event.kind));
        let level_matches =
            self.levels.is_empty() || self.levels.iter().any(|level| level == event.level);
        type_matches && level_matches
    }
}

/// An event as the CloudEvents 1.0 JSON a delivery carries: the type
/// under the `dev.muru.` prefix, the sequence as the id and as
/// `muruseq`, the level, and the message as data.
pub fn cloud_event(event: &Event, source: &str) -> serde_json::Value {
    serde_json::json!({
        "specversion": "1.0",
        "id": event.sequence.to_string(),
        "source": source,
        "type": format!("dev.muru.{}", event.kind),
        "subject": event.subject,
        "time": rfc3339(event.time_unix_ms),
        "datacontenttype": "application/json",
        "muruseq": event.sequence,
        "level": event.level,
        "data": {"message": event.message},
    })
}

/// The `X-Muru-Signature` value for a body: `sha256=` and the hex HMAC.
pub fn signature(secret: &[u8], body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let tag = ring::hmac::sign(&key, body);
    let hex: String = tag
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256={hex}")
}

/// The source attribute of every event from this node.
pub fn source() -> String {
    let hostname = crate::config::hostname();

    if hostname.is_empty() {
        "muru://localhost".to_owned()
    } else {
        format!("muru://{hostname}")
    }
}

fn rfc3339(unix_ms: u64) -> String {
    let time = std::time::UNIX_EPOCH + Duration::from_millis(unix_ms);
    let (date, hours, minutes, seconds, millis) = muru_edge::access::civil(time);
    format!("{date}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

fn backoff(failures: u32) -> Duration {
    let seconds = 1u64 << failures.saturating_sub(1).min(6);
    Duration::from_secs(seconds).min(MAX_BACKOFF)
}

type Connector = HttpsConnector<HttpConnector>;

/// Deliver events to the webhook until the event stream closes: the
/// backlog after the sink's cursor first, then each event as it happens.
/// A sink seen for the first time starts from now.
pub async fn run(webhook: Webhook, store: Arc<Store>, events: Events) {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client: Client<Connector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(connector);
    let source = source();
    let mut live = events.subscribe();

    if let Ok(None) = store.sink_cursor(&webhook.name) {
        let latest = store.last_event_sequence().unwrap_or(0);
        let _ = store.set_sink_cursor(&webhook.name, latest);
    }

    let mut failures = 0u32;
    let mut reported = false;

    loop {
        // Everything after the cursor, in order, each one until it is
        // taken; the cursor moves after each.

        loop {
            let cursor = match store.sink_cursor(&webhook.name) {
                Ok(cursor) => cursor.unwrap_or(0),
                Err(error) => {
                    tracing::warn!(sink = %webhook.name, %error, "reading the sink cursor failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            let pending = match store.events_after(cursor, 100) {
                Ok(pending) => pending,
                Err(error) => {
                    tracing::warn!(sink = %webhook.name, %error, "reading pending events failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            if pending.is_empty() {
                break;
            }

            for event in pending {
                if webhook.accepts(&event) {
                    while let Err(error) = deliver(&client, &webhook, &event, &source).await {
                        failures += 1;

                        if failures == FAILURES_TO_REPORT && !reported {
                            reported = true;
                            events.emit(
                                "events.sink.failing",
                                &webhook.name,
                                "warning",
                                &format!(
                                    "deliveries to {} are failing: {error}; retrying",
                                    webhook.url
                                ),
                            );
                        } else {
                            tracing::debug!(sink = %webhook.name, %error, "delivery failed; retrying");
                        }

                        tokio::time::sleep(backoff(failures)).await;
                    }

                    if reported {
                        reported = false;
                        events.emit(
                            "events.sink.recovered",
                            &webhook.name,
                            "notice",
                            &format!("deliveries to {} are getting through again", webhook.url),
                        );
                    }

                    failures = 0;
                }

                if let Err(error) = store.set_sink_cursor(&webhook.name, event.sequence) {
                    tracing::warn!(sink = %webhook.name, %error, "recording the sink cursor failed");
                }
            }
        }

        match live.recv().await {
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn deliver(
    client: &Client<Connector, Full<Bytes>>,
    webhook: &Webhook,
    event: &Event,
    source: &str,
) -> Result<(), String> {
    let body =
        serde_json::to_vec(&cloud_event(event, source)).map_err(|error| error.to_string())?;
    let request = Request::post(webhook.url.clone())
        .header(CONTENT_TYPE, CONTENT_TYPE_CLOUDEVENTS)
        .header(USER_AGENT, concat!("muru/", env!("CARGO_PKG_VERSION")))
        .header("x-muru-signature", signature(&webhook.secret, &body))
        .header("x-muru-event", format!("dev.muru.{}", event.kind))
        .header("x-muru-delivery", event.sequence.to_string())
        .body(Full::new(Bytes::from(body)))
        .map_err(|error| error.to_string())?;
    let response = tokio::time::timeout(DELIVERY_TIMEOUT, client.request(request))
        .await
        .map_err(|_| "timed out".to_owned())?
        .map_err(|error| error.to_string())?;

    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("the receiver answered {}", response.status()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, level: &'static str) -> Event {
        Event {
            sequence: 7,
            time_unix_ms: 1_761_300_000_123,
            kind: kind.to_owned(),
            subject: "shop@r1".to_owned(),
            level,
            message: "ready".to_owned(),
        }
    }

    fn webhook(types: &[&str], levels: &[&str]) -> Webhook {
        Webhook {
            name: "ops".to_owned(),
            url: "http://127.0.0.1:1/hook".parse().unwrap(),
            secret: b"s3cret".to_vec(),
            types: types.iter().map(|&type_| type_.to_owned()).collect(),
            levels: levels.iter().map(|&level| level.to_owned()).collect(),
        }
    }

    #[test]
    fn filters_match_types_by_pattern_and_levels_by_name() {
        assert!(webhook(&[], &[]).accepts(&event("release.ready", "info")));
        assert!(webhook(&["release.*"], &[]).accepts(&event("release.ready", "info")));
        assert!(!webhook(&["release.*"], &[]).accepts(&event("daemon.started", "info")));
        assert!(webhook(&[], &["warning", "error"]).accepts(&event("edge.failed", "error")));
        assert!(!webhook(&[], &["warning", "error"]).accepts(&event("release.ready", "info")));
    }

    #[test]
    fn the_cloud_event_carries_the_type_sequence_and_message() {
        let value = cloud_event(&event("release.ready", "info"), "muru://node1");
        assert_eq!(value["specversion"], "1.0");
        assert_eq!(value["type"], "dev.muru.release.ready");
        assert_eq!(value["id"], "7");
        assert_eq!(value["muruseq"], 7);
        assert_eq!(value["source"], "muru://node1");
        assert_eq!(value["subject"], "shop@r1");
        assert_eq!(value["level"], "info");
        assert_eq!(value["data"]["message"], "ready");
        assert_eq!(value["time"], "2025-10-24T10:00:00.123Z");
    }

    #[test]
    fn the_signature_is_the_hex_hmac_of_the_body() {
        // HMAC-SHA256 of an empty body with the key "key" is well known.

        assert_eq!(
            signature(b"key", b""),
            "sha256=5d5d139563c95b5967b9bd9a8c9b233a9dedb45072794cd232dc1b74832607d0"
        );
    }

    #[test]
    fn configuration_entries_are_checked() {
        let good = muru_config::EventSink {
            name: "ops".to_owned(),
            url: "https://example.com/hook".to_owned(),
            secret: Some("s".to_owned()),
            ..Default::default()
        };
        assert!(Webhook::from_config(&good).is_ok());
        let unsigned = muru_config::EventSink {
            secret: None,
            ..good.clone()
        };
        assert!(
            Webhook::from_config(&unsigned)
                .unwrap_err()
                .contains("secret")
        );
        let bad_url = muru_config::EventSink {
            url: "ftp://example.com".to_owned(),
            ..good.clone()
        };
        assert!(Webhook::from_config(&bad_url).unwrap_err().contains("http"));
        let bad_level = muru_config::EventSink {
            levels: vec!["loud".to_owned()],
            ..good
        };
        assert!(
            Webhook::from_config(&bad_level)
                .unwrap_err()
                .contains("loud")
        );
    }

    #[test]
    fn backoff_doubles_to_a_minute() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(backoff(20), Duration::from_secs(60));
    }
}
