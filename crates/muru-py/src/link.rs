//! The worker side of the edge-to-worker link: one request per connection,
//! connections kept open (design specification section 5).
//!
//! Each connection is served by one task. It reads a REQUEST frame, runs the
//! request on an application thread, forwards BODY frames to `wsgi.input`
//! until the request body ends or the edge sends ABORT, and writes the
//! response as RESPONSE, BODY and END frames, coalescing small chunks as
//! section 5.4 describes. When both directions have ended the connection is
//! idle again and the next REQUEST may follow. The edge closing the
//! connection cancels the request.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use muru_proto::frame::{self, flag, kind};
use muru_proto::worker::v1 as pb;
use prost::Message;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::pool::Shared;
use crate::wsgi::{self, Outgoing};

/// Errors from the link server.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Flush settings from section 5.4, fixed for the spike.
const COALESCE_MIN_BYTES: usize = 16 * 1024;
const COALESCE_MAX_DELAY: Duration = Duration::from_millis(10);

/// Payloads up to this size are copied into one buffer with their header.
const SMALL_PAYLOAD: usize = 16 * 1024;

type Reader = BufReader<OwnedReadHalf>;
type Writer = OwnedWriteHalf;

/// Bind the socket and serve link connections until SIGTERM or SIGINT, then
/// let the application threads finish their work.
pub fn serve(socket_path: &str, shared: Arc<Shared>) -> Result<(), BoxError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("muru-link")
        .enable_all()
        .build()?;

    let accepting = Arc::clone(&shared);
    let served: Result<(), BoxError> = runtime.block_on(async move {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)?;
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    tokio::spawn(serve_connection(stream, Arc::clone(&accepting)));
                }
                _ = &mut shutdown => break,
            }
        }
        Ok(())
    });

    // Requests already running finish on their threads while the runtime
    // still carries their responses; then everything stops.
    shared.pool.shutdown();
    drop(runtime);
    served
}

/// Resolve when SIGTERM or SIGINT arrives.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(_) => return,
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(signal) => signal,
        Err(_) => return,
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

async fn serve_connection(stream: tokio::net::UnixStream, shared: Arc<Shared>) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::with_capacity(32 * 1024, read_half);
    let mut writer = write_half;

    match frame::read(&mut reader).await {
        Ok(Some(hello)) if hello.kind == kind::HELLO => {
            let Ok(hello) = pb::Hello::decode(hello.payload) else {
                return;
            };
            if !same_major(&hello.protocol_version) {
                return;
            }
        }
        _ => return,
    }
    let hello = pb::Hello {
        protocol_version: pb::PROTOCOL_VERSION.to_owned(),
        worker_type: "wsgi".to_owned(),
        slots: u32::try_from(shared.threads).unwrap_or(u32::MAX),
        ..Default::default()
    };
    if frame::write(&mut writer, kind::HELLO, 0, hello.encode_to_vec().into())
        .await
        .is_err()
    {
        return;
    }

    loop {
        let request = match frame::read(&mut reader).await {
            Ok(Some(request)) if request.kind == kind::REQUEST => request,
            _ => return,
        };
        let request_end = request.is_end();
        let Ok(head) = pb::RequestHead::decode(request.payload) else {
            return;
        };
        if handle_request(&mut reader, &mut writer, head, request_end, &shared)
            .await
            .is_err()
        {
            return;
        }
    }
}

fn same_major(version: &str) -> bool {
    version.split('.').next() == pb::PROTOCOL_VERSION.split('.').next()
}

async fn handle_request(
    reader: &mut Reader,
    writer: &mut Writer,
    head: pb::RequestHead,
    request_end: bool,
    shared: &Arc<Shared>,
) -> io::Result<()> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (body_tx, body_rx) = mpsc::channel::<Bytes>(8);
    let (out_tx, out_rx) = mpsc::channel::<Outgoing>(32);
    shared.request_started();
    let worker_shared = Arc::clone(shared);
    let worker_cancelled = Arc::clone(&cancelled);
    let queued = shared.pool.spawn(Box::new(move |py| {
        wsgi::run(py, &worker_shared, head, body_rx, out_tx, worker_cancelled);
        worker_shared.request_finished();
    }));
    if !queued {
        shared.request_finished();
        return Err(io::Error::other("the worker is shutting down"));
    }

    let read_side = async {
        let mut body_tx = Some(body_tx);
        if request_end {
            return Ok(());
        }
        loop {
            let Some(next) = frame::read(reader).await? else {
                cancelled.store(true, Ordering::Relaxed);
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the edge closed the connection during the request body",
                ));
            };
            let end = next.is_end();
            match next.kind {
                kind::BODY => {
                    if let Some(tx) = &body_tx
                        && !next.payload.is_empty()
                        && tx.send(next.payload).await.is_err()
                    {
                        // The application stopped reading; discard the rest.
                        body_tx = None;
                    }
                    if end {
                        break;
                    }
                }
                kind::END => break,
                kind::ABORT => {
                    // The edge has no more use for the request. Whatever the
                    // application has not read is gone, and its iteration
                    // stops; the connection stays usable.
                    cancelled.store(true, Ordering::Relaxed);
                    break;
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected frame type {other} in a request body"),
                    ));
                }
            }
        }
        Ok(())
    };
    let write_side = write_response(out_rx, writer);
    let (read_result, write_result) = tokio::join!(read_side, write_side);
    read_result?;
    write_result
}

/// Turn the application's output into frames, coalescing small chunks.
async fn write_response(mut rx: mpsc::Receiver<Outgoing>, writer: &mut Writer) -> io::Result<()> {
    let mut pending_head: Option<Bytes> = None;
    let mut head_written = false;
    let mut content_length: Option<u64> = None;
    let mut sent: u64 = 0;
    let mut ended = false;
    let mut buffer = BytesMut::new();
    let mut deadline: Option<Instant> = None;
    let mut immediate = false;
    let mut first_block_sent = false;

    loop {
        let item = match deadline {
            Some(at) => tokio::select! {
                item = rx.recv() => Some(item),
                _ = tokio::time::sleep_until(at.into()) => None,
            },
            None => Some(rx.recv().await),
        };
        match item {
            None => {
                // The delay expired with data waiting.
                let data = buffer.split().freeze();
                sent += data.len() as u64;
                let end = content_length == Some(sent);
                write_body(writer, &mut pending_head, &mut head_written, data, end).await?;
                ended |= end;
                deadline = None;
            }
            Some(None) => {
                // The application finished.
                if ended {
                    return Ok(());
                }
                if !buffer.is_empty() {
                    let data = buffer.split().freeze();
                    write_body(writer, &mut pending_head, &mut head_written, data, true).await?;
                    return Ok(());
                }
                if let Some(head) = pending_head.take() {
                    return frame::write(writer, kind::RESPONSE, flag::END, head).await;
                }
                if !head_written {
                    // Nothing at all arrived: the bridge always sends a head, so
                    // this means the application thread died.
                    return Ok(());
                }
                let end = pb::End::default().encode_to_vec();
                return frame::write(writer, kind::END, 0, end.into()).await;
            }
            Some(Some(Outgoing::Head {
                status,
                headers,
                flush,
            })) => {
                immediate = flush == pb::FlushPolicy::Immediate;
                content_length = headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(b"content-length"))
                    .and_then(|(_, value)| std::str::from_utf8(value).ok()?.trim().parse().ok());
                let head = pb::ResponseHead {
                    status,
                    headers: headers
                        .into_iter()
                        .map(|(name, value)| pb::Header {
                            name: Bytes::from(name),
                            value: Bytes::from(value),
                        })
                        .collect(),
                    flush: flush as i32,
                };
                pending_head = Some(head.encode_to_vec().into());
                if content_length == Some(0) {
                    // No body will follow; send the head now and finish.
                    let head = pending_head.take().unwrap_or_default();
                    frame::write(writer, kind::RESPONSE, flag::END, head).await?;
                    head_written = true;
                    ended = true;
                }
            }
            Some(Some(Outgoing::Body(data))) => {
                if ended {
                    continue;
                }
                if immediate || !first_block_sent {
                    first_block_sent = true;
                    if !buffer.is_empty() {
                        let buffered = buffer.split().freeze();
                        sent += buffered.len() as u64;
                        write_body(
                            writer,
                            &mut pending_head,
                            &mut head_written,
                            buffered,
                            false,
                        )
                        .await?;
                    }
                    sent += data.len() as u64;
                    let end = content_length == Some(sent);
                    write_body(writer, &mut pending_head, &mut head_written, data, end).await?;
                    ended |= end;
                } else {
                    buffer.extend_from_slice(&data);
                    if buffer.len() >= COALESCE_MIN_BYTES {
                        let buffered = buffer.split().freeze();
                        sent += buffered.len() as u64;
                        let end = content_length == Some(sent);
                        write_body(writer, &mut pending_head, &mut head_written, buffered, end)
                            .await?;
                        ended |= end;
                        deadline = None;
                    } else if deadline.is_none() {
                        deadline = Some(Instant::now() + COALESCE_MAX_DELAY);
                    }
                }
            }
            Some(Some(Outgoing::Abort(message))) => {
                if ended {
                    return Ok(());
                }
                if !buffer.is_empty() {
                    let buffered = buffer.split().freeze();
                    write_body(
                        writer,
                        &mut pending_head,
                        &mut head_written,
                        buffered,
                        false,
                    )
                    .await?;
                }
                if let Some(head) = pending_head.take() {
                    frame::write(writer, kind::RESPONSE, 0, head).await?;
                }
                let end = pb::End {
                    error: message,
                    ..Default::default()
                }
                .encode_to_vec();
                return frame::write(writer, kind::END, 0, end.into()).await;
            }
        }
    }
}

/// Write a body chunk, with the response head in the same write when it is
/// still pending, and the END flag when this is the last chunk.
async fn write_body(
    writer: &mut Writer,
    pending_head: &mut Option<Bytes>,
    head_written: &mut bool,
    data: Bytes,
    end: bool,
) -> io::Result<()> {
    let flags = if end { flag::END } else { 0 };
    match pending_head.take() {
        Some(head) => {
            *head_written = true;
            if data.len() <= SMALL_PAYLOAD {
                let mut buffer =
                    BytesMut::with_capacity(2 * frame::HEADER_LEN + head.len() + data.len());
                frame::put_header(&mut buffer, kind::RESPONSE, 0, head.len());
                buffer.put_slice(&head);
                frame::put_header(&mut buffer, kind::BODY, flags, data.len());
                buffer.put_slice(&data);
                writer.write_all(&buffer).await
            } else {
                frame::write(writer, kind::RESPONSE, 0, head).await?;
                frame::write(writer, kind::BODY, flags, data).await
            }
        }
        None => frame::write(writer, kind::BODY, flags, data).await,
    }
}
