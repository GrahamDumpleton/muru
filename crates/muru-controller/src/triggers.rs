//! The overlays' external trigger (design specification section 9.10): a
//! signed webhook endpoint for monitoring and alerting systems, served on
//! the address of the daemon's `[overlay_webhook]` table. A request names
//! an overlay declared with `trigger.external = true` and activates or
//! ends it, as `muructl overlay activate` and `end` do.
//!
//! The request is `POST /v1/overlays/NAME:trigger` with a JSON body,
//! `{"action": "activate", "time": "2026-11-02T09:00:00Z", "for": "2h"}`,
//! and `X-Muru-Signature: sha256=HEX`, the HMAC-SHA256 of the body with
//! the table's secret, as the event sinks sign their deliveries. A body
//! whose time is more than five minutes from the daemon's clock is
//! refused, so a captured request cannot be replayed later.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::daemon::{Command, OverlayAction};
use crate::store::Store;

/// How far a request's time may be from the daemon's clock.
const SKEW_MS: u64 = 5 * 60 * 1000;

/// The largest body read.
const MAX_BODY: usize = 16 * 1024;

/// The endpoint's settings, from the `[overlay_webhook]` table.
#[derive(Debug, Clone)]
pub struct Webhook {
    /// Where it is served.
    pub address: SocketAddr,
    /// The key requests are signed with.
    pub secret: Vec<u8>,
}

impl Webhook {
    /// Check the table and read its secret.
    pub fn from_config(table: &muru_config::OverlayWebhook) -> Result<Self, String> {
        let address = table.listen.parse().map_err(|_| {
            format!(
                "listen = {:?} is not an address such as 0.0.0.0:9466",
                table.listen
            )
        })?;

        let secret = match (&table.secret, &table.secret_file) {
            (Some(_), Some(_)) => return Err("give secret or secret_file, not both".to_owned()),
            (Some(secret), None) => secret.as_bytes().to_vec(),
            (None, Some(path)) => std::fs::read_to_string(path)
                .map_err(|error| format!("reading {path}: {error}"))?
                .trim()
                .as_bytes()
                .to_vec(),
            (None, None) => {
                return Err("a secret (or secret_file) is needed to check requests".to_owned());
            }
        };

        if secret.is_empty() {
            return Err("the secret is empty".to_owned());
        }

        Ok(Self { address, secret })
    }
}

/// What a request asks.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Trigger {
    /// activate or end.
    action: String,
    /// When it was sent, as an RFC 3339 time.
    time: String,
    /// For activate: how long the overlay stays active.
    #[serde(default, rename = "for")]
    duration: Option<String>,
    /// For activate: until when, as an RFC 3339 time.
    #[serde(default)]
    until: Option<String>,
}

/// Serve the endpoint on the listener until accepting fails.
pub async fn serve(
    listener: TcpListener,
    secret: Vec<u8>,
    store: Arc<Store>,
    commands: mpsc::Sender<Command>,
) -> anyhow::Result<()> {
    let secret = Arc::new(secret);

    loop {
        let (stream, _) = listener.accept().await?;
        let secret = Arc::clone(&secret);
        let store = Arc::clone(&store);
        let commands = commands.clone();

        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let secret = Arc::clone(&secret);
                let store = Arc::clone(&store);
                let commands = commands.clone();
                async move { Ok::<_, Infallible>(handle(request, &secret, &store, &commands).await) }
            });

            if let Err(error) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(%error, "an overlay webhook connection failed");
            }
        });
    }
}

async fn handle(
    request: Request<Incoming>,
    secret: &[u8],
    store: &Store,
    commands: &mpsc::Sender<Command>,
) -> Response<Full<Bytes>> {
    let path = request.uri().path().to_owned();

    let Some(name) = path
        .strip_prefix("/v1/overlays/")
        .and_then(|rest| rest.strip_suffix(":trigger"))
    else {
        return answer(
            StatusCode::NOT_FOUND,
            "not_found",
            "requests go to POST /v1/overlays/NAME:trigger",
        );
    };

    if request.method() != Method::POST {
        return answer(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "only POST triggers an overlay",
        );
    }

    let signature = request
        .headers()
        .get("x-muru-signature")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let body = match Limited::new(request.into_body(), MAX_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return answer(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "the body is over 16 KiB",
            );
        }
    };

    if !verify(secret, &body, signature.as_deref()) {
        return answer(
            StatusCode::UNAUTHORIZED,
            "bad_signature",
            "X-Muru-Signature is not sha256= and the body's HMAC with the webhook's secret",
        );
    }

    let trigger: Trigger = match serde_json::from_slice(&body) {
        Ok(trigger) => trigger,
        Err(error) => return answer(StatusCode::BAD_REQUEST, "bad_request", &error.to_string()),
    };

    let now = crate::model::now_unix_ms();

    match moment(&trigger.time) {
        Ok(sent) if sent.abs_diff(now) <= SKEW_MS => {}
        Ok(_) => {
            return answer(
                StatusCode::UNAUTHORIZED,
                "stale",
                "time is more than five minutes from the daemon's clock",
            );
        }
        Err(error) => {
            return answer(
                StatusCode::BAD_REQUEST,
                "bad_request",
                &format!("time: {error}"),
            );
        }
    }

    let external = store.overlays().ok().and_then(|overlays| {
        overlays
            .into_iter()
            .find(|(declared, _)| declared == name)
            .map(|(_, overlay)| {
                overlay
                    .trigger
                    .is_some_and(|trigger| trigger.external == Some(true))
            })
    });

    if external != Some(true) {
        return answer(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("no overlay named {name} takes the external trigger"),
        );
    }

    let action = match action(&trigger, now) {
        Ok(action) => action,
        Err(error) => return answer(StatusCode::BAD_REQUEST, "bad_request", &error),
    };

    let (done_tx, done_rx) = oneshot::channel();
    let sent = commands
        .send(Command::Overlay {
            name: name.to_owned(),
            action,
            external: true,
            done: done_tx,
        })
        .await;

    match (sent, done_rx.await) {
        (Ok(()), Ok(Ok(message))) => {
            let body = serde_json::json!({ "overlay": name, "message": message });
            respond(StatusCode::OK, body.to_string())
        }
        (Ok(()), Ok(Err(why))) => answer(StatusCode::CONFLICT, "refused", &why),
        _ => answer(
            StatusCode::SERVICE_UNAVAILABLE,
            "stopping",
            "the daemon is stopping",
        ),
    }
}

/// The overlay action a request asks for.
fn action(trigger: &Trigger, now: u64) -> Result<OverlayAction, String> {
    match trigger.action.as_str() {
        "activate" => {
            let until = match (&trigger.duration, &trigger.until) {
                (Some(_), Some(_)) => return Err("give for or until, not both".to_owned()),
                (Some(duration), None) => {
                    let duration = muru_config::parse_duration(duration)
                        .map_err(|error| format!("for: {error}"))?;
                    Some(now + duration.as_millis() as u64)
                }
                (None, Some(until)) => {
                    Some(moment(until).map_err(|error| format!("until: {error}"))?)
                }
                (None, None) => None,
            };

            Ok(OverlayAction::Activate { until })
        }
        "end" if trigger.duration.is_none() && trigger.until.is_none() => Ok(OverlayAction::End),
        "end" => Err("for and until go with activate".to_owned()),
        other => Err(format!("action {other:?} is not activate or end")),
    }
}

/// An RFC 3339 time as Unix milliseconds.
fn moment(text: &str) -> Result<u64, String> {
    muru_config::parse_moment(text, Some("UTC")).map(|millis| millis.max(0) as u64)
}

/// Whether `signature` is `sha256=` and the hex HMAC-SHA256 of the body,
/// compared in constant time.
fn verify(secret: &[u8], body: &[u8], signature: Option<&str>) -> bool {
    let Some(tag) = signature
        .and_then(|signature| signature.strip_prefix("sha256="))
        .and_then(hex)
    else {
        return false;
    };

    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    ring::hmac::verify(&key, body, &tag).is_ok()
}

fn hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }

    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// A refusal as JSON: a reason and a sentence.
fn answer(status: StatusCode, reason: &str, message: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({ "error": reason, "message": message });
    respond(status, body.to_string())
}

fn respond(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_is_the_sink_scheme() {
        let body = br#"{"action":"end","time":"2026-11-02T09:00:00Z"}"#;
        let signed = crate::sinks::signature(b"s3cret", body);
        assert!(verify(b"s3cret", body, Some(&signed)));
        assert!(!verify(b"other", body, Some(&signed)));
        assert!(!verify(b"s3cret", b"{}", Some(&signed)));
        assert!(!verify(b"s3cret", body, Some("sha256=zz")));
        assert!(!verify(b"s3cret", body, None));
    }

    #[test]
    fn actions_take_a_time_to_end() {
        let trigger = |text: &str| -> Trigger { serde_json::from_str(text).unwrap() };
        let now = 1_000_000;
        assert_eq!(
            action(
                &trigger(r#"{"action":"activate","time":"x","for":"2s"}"#),
                now
            ),
            Ok(OverlayAction::Activate {
                until: Some(now + 2000)
            })
        );
        assert_eq!(
            action(&trigger(r#"{"action":"end","time":"x"}"#), now),
            Ok(OverlayAction::End)
        );
        assert!(action(&trigger(r#"{"action":"promote","time":"x"}"#), now).is_err());
        assert!(action(&trigger(r#"{"action":"end","time":"x","for":"2s"}"#), now).is_err());
    }
}
