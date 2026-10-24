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

/// Where the socket stands on its way to its end.
#[derive(Default)]
struct Closing {
    /// The client's stream has ended: its close frame was read and the
    /// reply flushed, or it went away.
    stream_done: bool,
    /// The worker's CLOSE has arrived.
    worker_closed: bool,
    /// A CLOSE has been sent to the worker.
    told_worker: bool,
    /// A close frame has been sent to the client.
    told_client: bool,
}

impl Closing {
    /// Both sides have closed in order: the link connection can be reused.
    fn complete(&self) -> bool {
        self.stream_done && self.worker_closed && self.told_worker
    }

    /// One side has begun closing, so the other is waited for, not for ever.
    fn winding_down(&self) -> bool {
        self.stream_done || self.worker_closed || self.told_worker || self.told_client
    }
}

/// Run one WebSocket to its end: messages from the client become MESSAGE
/// frames and the worker's MESSAGE frames go to the client, until each
/// side has sent its CLOSE and the client's stream has ended. The link
/// connection goes back to the pool when both closed in order, and is
/// dropped when either side broke off instead.
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
    while !closing.complete() {
        // Once a side has begun closing, only the rest of the handshake is
        // awaited, and not for ever.

        let wait = if closing.winding_down() {
            limits.response_idle_timeout
        } else {
            limits.websocket_idle_timeout
        };
        let step = tokio::time::timeout(wait, async {
            tokio::select! {
                incoming = socket.next(), if !closing.stream_done => Step::Client(incoming),
                next = frame::read(&mut reader), if !closing.worker_closed => Step::Worker(next),
                _ = ping.tick() => Step::Ping,
            }
        })
        .await;

        match step {
            Err(_) if closing.winding_down() => {
                // The other side never finished closing: give up on the link.

                intact = false;
                break;
            }
            Err(_) => {
                // Nothing from the client for the idle timeout.

                closing.told_client |= close_client(&mut socket, 1001, "idle").await.is_ok();
                if !closing.told_worker {
                    closing.told_worker = send_close(&mut writer, 1001, "idle").await.is_ok();
                }
                last_activity = Instant::now();
            }
            Ok(Step::Ping) => {
                if last_activity.elapsed() >= limits.websocket_ping_interval
                    && !closing.winding_down()
                    && socket.send(Message::Ping(Bytes::new())).await.is_err()
                {
                    closing.stream_done = true;
                }
            }
            Ok(Step::Client(incoming)) => {
                last_activity = Instant::now();
                let outcome = client_step(incoming, &mut socket, &mut writer, &mut closing).await;
                match outcome {
                    Ok(relayed) => bytes += relayed,
                    Err(()) => {
                        intact = false;
                        break;
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
                    if !closing.stream_done && socket.send(message).await.is_err() {
                        // The client is gone; the worker learns by CLOSE.

                        closing.stream_done = true;
                        if !closing.told_worker {
                            closing.told_worker =
                                send_close(&mut writer, 1006, "the client went away")
                                    .await
                                    .is_ok();
                        }
                    }
                }
                Ok(Some(next)) if next.kind == kind::CLOSE => {
                    // The application closed: pass it on, answer the worker
                    // with the edge's own CLOSE, and let the client's echo
                    // end its stream.

                    let close = pb::Close::decode(next.payload).unwrap_or_default();
                    let code = u16::try_from(close.code).unwrap_or(1000);
                    closing.worker_closed = true;
                    if !closing.told_client && !closing.stream_done {
                        closing.told_client = true;
                        let _ = close_client(&mut socket, code, &close.reason).await;
                    }
                    if !closing.told_worker {
                        if send_close(&mut writer, code, &close.reason).await.is_err() {
                            intact = false;
                            break;
                        }
                        closing.told_worker = true;
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
    }

    // A socket that ended in order leaves the link idle for the next
    // request; one that broke off takes its connection with it.

    let intact = intact && closing.complete();
    let _ = socket.close(None).await;
    back.give_writer(intact.then_some(writer));
    back.give_reader(intact.then_some(reader));
    finish(recording, bytes);
}

/// Handle one item from the client's stream: a message to relay, a close
/// frame, a protocol failure or the end of the stream. Returns the bytes
/// relayed, or Err when the link writer failed.
async fn client_step<S>(
    incoming: Option<Result<Message, WsError>>,
    socket: &mut WebSocketStream<S>,
    writer: &mut Writer,
    closing: &mut Closing,
) -> Result<u64, ()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Tell the worker the client has gone, once.

    async fn tell_worker(
        writer: &mut Writer,
        closing: &mut Closing,
        code: u16,
        reason: &str,
    ) -> Result<(), ()> {
        if closing.told_worker {
            return Ok(());
        }
        closing.told_worker = true;
        send_close(writer, code, reason).await.map_err(|_| ())
    }

    match incoming {
        Some(Ok(Message::Text(text))) => {
            let payload = Bytes::from(text.as_str().to_owned());
            let size = payload.len() as u64;
            if closing.told_worker {
                return Ok(0);
            }
            frame::write(writer, kind::MESSAGE, flag::TEXT, payload)
                .await
                .map_err(|_| ())?;
            Ok(size)
        }
        Some(Ok(Message::Binary(data))) => {
            let size = data.len() as u64;
            if closing.told_worker {
                return Ok(0);
            }
            frame::write(writer, kind::MESSAGE, 0, data)
                .await
                .map_err(|_| ())?;
            Ok(size)
        }
        Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
            // tungstenite queues the pong itself; flushing sends it now.

            let _ = socket.flush().await;
            Ok(0)
        }
        Some(Ok(Message::Close(close))) => {
            // tungstenite queues the reply itself; the stream ends once it
            // has been flushed and read to its end.

            let (code, reason) = close
                .map(|close| (u16::from(close.code), close.reason.to_string()))
                .unwrap_or((1005, String::new()));
            let _ = socket.flush().await;
            closing.told_client = true;
            tell_worker(writer, closing, code, &reason).await?;
            Ok(0)
        }
        Some(Err(WsError::Capacity(_))) => {
            // Section 4.18: a message over the limit is 1009.

            let _ = close_client(socket, 1009, "message too large").await;
            closing.told_client = true;
            tell_worker(writer, closing, 1009, "message too large").await?;
            Ok(0)
        }
        Some(Err(WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake))) => {
            // Gone without a close frame: 1006 to the application.

            closing.stream_done = true;
            tell_worker(writer, closing, 1006, "the client went away").await?;
            Ok(0)
        }
        Some(Err(WsError::Protocol(error))) => {
            // A protocol failure: close with 1002 and end the stream.

            let reason = error.to_string();
            let _ = close_client(socket, 1002, &reason).await;
            closing.told_client = true;
            closing.stream_done = true;
            tell_worker(writer, closing, 1002, &reason).await?;
            Ok(0)
        }
        Some(Err(WsError::Utf8(_))) => {
            let _ = close_client(socket, 1007, "invalid UTF-8").await;
            closing.told_client = true;
            closing.stream_done = true;
            tell_worker(writer, closing, 1007, "invalid UTF-8").await?;
            Ok(0)
        }
        Some(Err(_)) | None => {
            // The stream has ended, in order after a close or not.

            closing.stream_done = true;
            tell_worker(writer, closing, 1006, "the client went away").await?;
            Ok(0)
        }
    }
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
