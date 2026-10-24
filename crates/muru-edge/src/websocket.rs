//! WebSockets at the edge (design specification sections 4.18 and 5.6).
//!
//! The edge owns the wire protocol: it recognises the upgrade, completes
//! the handshake once the worker has accepted with a 101, and then relays
//! whole messages between the client's socket and the link, where each
//! message is one MESSAGE frame and each end of the socket one CLOSE frame.
//! Masking, fragment reassembly, pings and pongs never reach the worker.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, UPGRADE};
use http::{HeaderValue, Response, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use muru_proto::frame::{self, flag, kind};
use muru_proto::worker::v1 as pb;
use prost::Message as _;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::Limits;
use crate::link::{Reader, Recording, Return, Writer};

/// A request that asks to become a WebSocket.
pub(crate) struct Upgrade {
    /// The client's Sec-WebSocket-Key, for HTTP/1.1; HTTP/2 has none.
    key: Option<String>,
    /// How the client will see the upgrade: a 101 over HTTP/1.1, or a 200
    /// to its extended CONNECT over HTTP/2.
    http2: bool,
    /// Where hyper hands over the connection once the response is sent.
    on_upgrade: OnUpgrade,
}

/// Recognise a WebSocket upgrade: the HTTP/1.1 Upgrade dance, or the
/// extended CONNECT of RFC 8441 over HTTP/2. Takes the upgrade handle out
/// of the request so the relay can use it.
pub(crate) fn detect(parts: &mut http::request::Parts) -> Option<Upgrade> {
    let header = |name: http::HeaderName| {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    if header(http::header::SEC_WEBSOCKET_VERSION) != Some("13") {
        return None;
    }

    // HTTP/2: CONNECT with :protocol = websocket. HTTP/1.1: GET with
    // Upgrade: websocket named in the Connection header and a key.
    let http2 = parts.method == http::Method::CONNECT
        && parts
            .extensions
            .get::<hyper::ext::Protocol>()
            .is_some_and(|protocol| protocol.as_str().eq_ignore_ascii_case("websocket"));
    let http1 = parts.method == http::Method::GET
        && header(UPGRADE).is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && header(CONNECTION).is_some_and(|value| {
            value
                .split(',')
                .any(|option| option.trim().eq_ignore_ascii_case("upgrade"))
        });
    let key = header(http::header::SEC_WEBSOCKET_KEY).map(str::to_owned);
    if !(http2 || (http1 && key.is_some())) {
        return None;
    }

    let on_upgrade = parts.extensions.remove::<OnUpgrade>()?;
    Some(Upgrade {
        key: if http2 { None } else { key },
        http2,
        on_upgrade,
    })
}

/// The response that completes the handshake, once the worker has
/// answered 101: the headers the application chose (its subprotocol, for
/// one) plus what the protocol requires.
pub(crate) fn accepted_response(
    upgrade: &Upgrade,
    worker_headers: Vec<pb::Header>,
) -> Response<crate::ResponseBody> {
    let status = if upgrade.http2 {
        StatusCode::OK
    } else {
        StatusCode::SWITCHING_PROTOCOLS
    };
    let mut builder = Response::builder().status(status);
    if let Some(headers) = builder.headers_mut() {
        crate::copy_headers(headers, worker_headers);
        if let Some(key) = &upgrade.key {
            let accept =
                tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
            headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
            headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
            if let Ok(value) = HeaderValue::from_str(&accept) {
                headers.insert(SEC_WEBSOCKET_ACCEPT, value);
            }
        }
    }

    let empty = Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync();
    builder.body(empty).unwrap_or_else(|_| {
        Response::new(Empty::new().map_err(|never| match never {}).boxed_unsync())
    })
}

/// What the relay owns for the life of one socket.
pub(crate) struct Relay {
    pub upgrade: Upgrade,
    pub reader: Reader,
    pub writer: Writer,
    pub back: Arc<Return>,
    pub recording: Option<Recording>,
    pub limits: Arc<Limits>,
}

/// The two CLOSE frames of a socket, one per side.
#[derive(Default)]
struct Closing {
    client: bool,
    worker: bool,
}

/// Run one WebSocket to its end: messages from the client become MESSAGE
/// frames and the worker's MESSAGE frames go to the client, until each
/// side has sent its CLOSE. The link connection goes back to the pool when
/// both have, and is dropped when either side broke off instead.
pub(crate) async fn relay(relay: Relay) {
    let Relay {
        upgrade,
        mut reader,
        mut writer,
        back,
        recording,
        limits,
    } = relay;

    // hyper hands the connection over once the 101 (or 200) has gone out.
    let upgraded = match upgrade.on_upgrade.await {
        Ok(upgraded) => upgraded,
        Err(error) => {
            tracing::debug!(%error, "the WebSocket upgrade failed");
            let _ = send_close(&mut writer, 1006, "the client went away").await;
            back.give_writer(None);
            back.give_reader(None);
            finish(recording, 0);
            return;
        }
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(limits.websocket_max_message_size))
        .max_frame_size(Some(limits.websocket_max_message_size));
    let mut socket =
        WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config)).await;

    let mut closing = Closing::default();
    let mut intact = true;
    let mut bytes: u64 = 0;
    let mut last_activity = Instant::now();
    let mut ping = tokio::time::interval(limits.websocket_ping_interval);
    ping.tick().await;
    loop {
        // Once a side has closed, only the other side's CLOSE is awaited,
        // and not for ever.
        let wait = if closing.client || closing.worker {
            limits.response_idle_timeout
        } else {
            limits.websocket_idle_timeout
        };
        let step = tokio::time::timeout(wait, async {
            tokio::select! {
                incoming = socket.next(), if !closing.client => Step::Client(incoming),
                next = frame::read(&mut reader), if !closing.worker => Step::Worker(next),
                _ = ping.tick() => Step::Ping,
            }
        })
        .await;

        match step {
            Err(_) if closing.client || closing.worker => {
                // The other side never sent its CLOSE: give up on the link.
                intact = false;
                break;
            }
            Err(_) => {
                // Nothing from the client for the idle timeout.
                let _ = close_client(&mut socket, 1001, "idle").await;
                let _ = send_close(&mut writer, 1001, "idle").await;
                closing.client = true;
                last_activity = Instant::now();
            }
            Ok(Step::Ping) => {
                if last_activity.elapsed() >= limits.websocket_ping_interval
                    && socket.send(Message::Ping(Bytes::new())).await.is_err()
                {
                    intact = false;
                    break;
                }
            }
            Ok(Step::Client(incoming)) => {
                last_activity = Instant::now();
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        bytes += text.len() as u64;
                        let payload = Bytes::from(text.as_str().to_owned());
                        if frame::write(&mut writer, kind::MESSAGE, flag::TEXT, payload)
                            .await
                            .is_err()
                        {
                            intact = false;
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        bytes += data.len() as u64;
                        if frame::write(&mut writer, kind::MESSAGE, 0, data)
                            .await
                            .is_err()
                        {
                            intact = false;
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(close))) => {
                        // tungstenite echoes the close frame itself.
                        let (code, reason) = close
                            .map(|close| (u16::from(close.code), close.reason.to_string()))
                            .unwrap_or((1005, String::new()));
                        if send_close(&mut writer, code, &reason).await.is_err() {
                            intact = false;
                            break;
                        }
                        closing.client = true;
                    }
                    Some(Err(WsError::Capacity(_))) => {
                        // Section 4.18: a message over the limit is 1009.
                        let _ = close_client(&mut socket, 1009, "message too large").await;
                        let _ = send_close(&mut writer, 1009, "message too large").await;
                        closing.client = true;
                    }
                    Some(Err(WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake))) => {
                        // Gone without a close frame: 1006 to the application.
                        let _ = send_close(&mut writer, 1006, "the client went away").await;
                        closing.client = true;
                    }
                    Some(Err(WsError::Protocol(error))) => {
                        let reason = error.to_string();
                        let _ = close_client(&mut socket, 1002, &reason).await;
                        let _ = send_close(&mut writer, 1002, &reason).await;
                        closing.client = true;
                    }
                    Some(Err(_)) | None => {
                        // The client went away without closing.
                        let _ = send_close(&mut writer, 1006, "the client went away").await;
                        closing.client = true;
                    }
                }
            }
            Ok(Step::Worker(next)) => match next {
                Ok(Some(next)) if next.kind == kind::MESSAGE => {
                    bytes += next.payload.len() as u64;
                    let message = if next.flags & flag::TEXT != 0 {
                        match std::str::from_utf8(&next.payload) {
                            Ok(text) => Message::Text(text.into()),
                            Err(_) => Message::Binary(next.payload),
                        }
                    } else {
                        Message::Binary(next.payload)
                    };
                    if socket.send(message).await.is_err() {
                        // The client is gone; the worker learns by CLOSE.
                        let _ = send_close(&mut writer, 1006, "the client went away").await;
                        closing.client = true;
                    }
                }
                Ok(Some(next)) if next.kind == kind::CLOSE => {
                    let close = pb::Close::decode(next.payload).unwrap_or_default();
                    closing.worker = true;
                    if !closing.client {
                        let code = u16::try_from(close.code).unwrap_or(1000);
                        let _ = close_client(&mut socket, code, &close.reason).await;
                        if send_close(&mut writer, code, &close.reason).await.is_err() {
                            intact = false;
                            break;
                        }
                        closing.client = true;
                    }
                }
                Ok(Some(next)) => {
                    tracing::warn!(kind = next.kind, "unexpected frame on a WebSocket link");
                    intact = false;
                    break;
                }
                Ok(None) | Err(_) => {
                    let _ = close_client(&mut socket, 1011, "the worker went away").await;
                    intact = false;
                    break;
                }
            },
        }

        if closing.client && closing.worker {
            break;
        }
    }

    // A socket that ended in order leaves the link idle for the next
    // request; one that broke off takes its connection with it.
    let _ = socket.close(None).await;
    back.give_writer(intact.then_some(writer));
    back.give_reader(intact.then_some(reader));
    finish(recording, bytes);
}

enum Step {
    Client(Option<Result<Message, WsError>>),
    Worker(std::io::Result<Option<frame::Frame>>),
    Ping,
}

async fn send_close(writer: &mut Writer, code: u16, reason: &str) -> std::io::Result<()> {
    let close = pb::Close {
        code: u32::from(code),
        reason: reason.to_owned(),
    };
    frame::write(writer, kind::CLOSE, 0, close.encode_to_vec().into()).await
}

async fn close_client<S>(
    socket: &mut WebSocketStream<S>,
    code: u16,
    reason: &str,
) -> Result<(), WsError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_owned().into(),
    };
    socket.close(Some(frame)).await
}

/// Record the socket in the access log as the 101 it began with.
fn finish(recording: Option<Recording>, bytes: u64) {
    if let Some(recording) = recording {
        recording.complete(101, "", bytes);
    }
}
