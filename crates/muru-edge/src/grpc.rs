//! gRPC calls (design specification section 6.13). A request whose
//! content type is gRPC or gRPC-Web, to a release whose workers registered
//! gRPC methods, is a call: it shares listeners, virtual hosts, routes and
//! their policies with HTTP, and each call is balanced across the workers
//! on its own.
//!
//! The edge unframes and decompresses each request message and hands it
//! to the worker whole, in a MESSAGE frame; it frames the worker's
//! messages for the client and sends the status the worker ends the call
//! with in trailers, or for gRPC-Web at the end of the body. It bounds the
//! call by its grpc-timeout, answers the standard health service itself,
//! from whether the release has workers, and answers a method nobody
//! registered with UNIMPLEMENTED.

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::StreamExt;
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use http_body::Frame;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::body::Incoming;
use muru_proto::frame::{self, kind};
use muru_proto::worker::v1 as pb;
use prost::Message;

use crate::link::{Context, LinkPool, Reader, Recording, Return, Writer};
use crate::{BoxError, ResponseBody};

/// The status codes the edge itself ends calls with.
mod code {
    pub const OK: u32 = 0;
    pub const UNKNOWN: u32 = 2;
    pub const DEADLINE_EXCEEDED: u32 = 4;
    pub const NOT_FOUND: u32 = 5;
    pub const RESOURCE_EXHAUSTED: u32 = 8;
    pub const UNIMPLEMENTED: u32 = 12;
    pub const INTERNAL: u32 = 13;
    pub const UNAVAILABLE: u32 = 14;
}

/// The standard health service's methods (grpc.health.v1).
const HEALTH_CHECK: &str = "/grpc.health.v1.Health/Check";
const HEALTH_WATCH: &str = "/grpc.health.v1.Health/Watch";

/// The flag on a gRPC-Web frame that carries the trailers.
const TRAILER_FRAME: u8 = 0x80;

/// How a call is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// gRPC over HTTP/2, the status in trailers.
    Grpc,
    /// gRPC-Web: the trailers in a frame at the end of the body.
    Web,
    /// gRPC-Web with the body base64 encoded.
    WebText,
}

impl Protocol {
    /// The protocol a request's content type names, if it is gRPC.
    pub fn of(headers: &HeaderMap) -> Option<Self> {
        let content_type = headers.get(CONTENT_TYPE)?.to_str().ok()?;
        let essence = content_type.split(';').next()?.trim().to_ascii_lowercase();

        match essence.split('+').next()? {
            "application/grpc" => Some(Self::Grpc),
            "application/grpc-web" => Some(Self::Web),
            "application/grpc-web-text" => Some(Self::WebText),
            _ => None,
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Grpc => "application/grpc",
            Self::Web => "application/grpc-web+proto",
            Self::WebText => "application/grpc-web-text+proto",
        }
    }

    pub(crate) fn is_web(self) -> bool {
        self != Self::Grpc
    }
}

/// How request messages are compressed (grpc-encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Identity,
    Gzip,
    /// The zlib format, as gRPC names it.
    Deflate,
}

impl Encoding {
    fn of(headers: &HeaderMap) -> Result<Self, String> {
        let Some(value) = headers.get("grpc-encoding") else {
            return Ok(Self::Identity);
        };

        match value.to_str().unwrap_or_default().trim() {
            "" | "identity" => Ok(Self::Identity),
            "gzip" => Ok(Self::Gzip),
            "deflate" => Ok(Self::Deflate),
            other => Err(format!("grpc-encoding {other} is not supported")),
        }
    }
}

/// A status the call ends with, from the edge rather than the worker.
#[derive(Debug, Clone)]
struct Status {
    code: u32,
    message: String,
}

impl Status {
    fn new(code: u32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn trailers(&self) -> Vec<(HeaderName, HeaderValue)> {
        let mut trailers = vec![(
            HeaderName::from_static("grpc-status"),
            HeaderValue::from(self.code),
        )];

        if !self.message.is_empty()
            && let Ok(value) = HeaderValue::from_bytes(&percent_encode(&self.message))
        {
            trailers.push((HeaderName::from_static("grpc-message"), value));
        }

        trailers
    }
}

/// Section 6.13: the origins whose pages may call the release's methods
/// through gRPC-Web, for CORS; none when empty, and every one for `*`.
#[derive(Debug, Default)]
pub struct WebOrigins {
    any: bool,
    origins: Vec<String>,
}

/// The request headers a gRPC-Web client sends, allowed when a preflight
/// names none.
const WEB_HEADERS: &str = "content-type, x-grpc-web, x-user-agent, grpc-timeout";

/// The response headers a page may read: the status of a trailers-only
/// answer.
const WEB_EXPOSED: &str = "grpc-status, grpc-message, grpc-status-details-bin";

impl WebOrigins {
    /// The origins as the release names them: `*`, or scheme, host and
    /// port, compared without regard to case.
    pub fn new(origins: &[String]) -> Self {
        Self {
            any: origins.iter().any(|origin| origin == "*"),
            origins: origins
                .iter()
                .filter(|origin| *origin != "*")
                .map(|origin| origin.trim_end_matches('/').to_ascii_lowercase())
                .collect(),
        }
    }

    /// The request's Origin, when CORS is on and it is allowed.
    fn allowed<'a>(&self, headers: &'a HeaderMap) -> Option<&'a HeaderValue> {
        if !self.any && self.origins.is_empty() {
            return None;
        }

        let origin = headers.get(http::header::ORIGIN)?;
        let text = origin.to_str().ok()?.to_ascii_lowercase();
        (self.any || self.origins.contains(&text)).then_some(origin)
    }

    /// Whether a request is a CORS preflight these origins answer: an
    /// OPTIONS with an Origin and Access-Control-Request-Method.
    pub(crate) fn is_preflight(&self, parts: &http::request::Parts) -> bool {
        (self.any || !self.origins.is_empty())
            && parts.method == http::Method::OPTIONS
            && parts.headers.contains_key(http::header::ORIGIN)
            && parts
                .headers
                .contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD)
    }

    /// The answer to a preflight: 204 allowing POST with the headers asked
    /// for, for a day, or 403 to an origin not named.
    pub(crate) fn preflight(&self, parts: &http::request::Parts) -> Response<ResponseBody> {
        let mut response = Response::new(empty());
        let headers = response.headers_mut();
        headers.insert(http::header::VARY, HeaderValue::from_static("Origin"));

        let Some(origin) = self.allowed(&parts.headers) else {
            *response.status_mut() = StatusCode::FORBIDDEN;
            return response;
        };

        *response.status_mut() = StatusCode::NO_CONTENT;
        let headers = response.headers_mut();
        headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        headers.insert(
            http::header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("POST"),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            parts
                .headers
                .get(http::header::ACCESS_CONTROL_REQUEST_HEADERS)
                .cloned()
                .unwrap_or(HeaderValue::from_static(WEB_HEADERS)),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("86400"),
        );
        response
    }

    /// Add the CORS headers to a gRPC-Web answer to an allowed origin.
    pub(crate) fn decorate(&self, request: &HeaderMap, response: &mut Response<ResponseBody>) {
        let Some(origin) = self.allowed(request) else {
            return;
        };

        let headers = response.headers_mut();
        headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        headers.insert(
            http::header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(WEB_EXPOSED),
        );
        headers.append(http::header::VARY, HeaderValue::from_static("Origin"));
    }
}

/// An empty response body.
fn empty() -> ResponseBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// Serve one call.
pub(crate) async fn handle(
    parts: &http::request::Parts,
    body: Option<Incoming>,
    pool: Arc<LinkPool>,
    context: &Context<'_>,
    protocol: Protocol,
) -> Response<ResponseBody> {
    let recording = Recording::new(context, parts);
    let path = parts.uri.path().to_owned();
    let limits = &context.limits;

    let encoding = match Encoding::of(&parts.headers) {
        Ok(encoding) => encoding,
        Err(message) => {
            let mut response = trailers_only(protocol, &Status::new(code::UNIMPLEMENTED, message));
            response.headers_mut().insert(
                "grpc-accept-encoding",
                HeaderValue::from_static("identity, deflate, gzip"),
            );
            return recorded(recording, response);
        }
    };

    if path == HEALTH_CHECK || path == HEALTH_WATCH {
        let response = health(body, &pool, protocol, encoding, path == HEALTH_WATCH).await;
        return recorded(recording, response);
    }

    let Some(method) = pool.grpc_method(&path).cloned() else {
        let status = Status::new(code::UNIMPLEMENTED, format!("Method not found: {path}"));
        return recorded(recording, trailers_only(protocol, &status));
    };

    // A unary call is bounded by the request deadline too; a streaming
    // one only by its grpc-timeout, and by the idle timeout while it
    // streams.

    let streaming = method.client_streaming || method.server_streaming;
    let mut deadline = parts
        .headers
        .get("grpc-timeout")
        .and_then(|value| parse_timeout(value.to_str().ok()?))
        .map(|timeout| context.arrived + timeout);

    if !streaming {
        let request_deadline = context.arrived + limits.request_deadline;
        deadline = Some(deadline.map_or(request_deadline, |at| at.min(request_deadline)));
    }

    let mut head = crate::request_head(parts, context);
    head.scope = pb::Scope::Grpc as i32;
    head.mount = method.mount.clone();
    head.script_name = String::new();
    head.raw_path = Bytes::from(path.clone());
    head.content_length = None;
    head.expect_continue = false;
    head.max_body_size = limits.max_body_size;
    head.deadline_unix_ms = deadline.map_or(0, crate::link::unix_deadline);

    let queued = Instant::now();
    let queue_timeout = deadline.map_or(limits.queue_timeout, |at| {
        limits
            .queue_timeout
            .min(at.saturating_duration_since(queued))
    });

    let lease = match tokio::time::timeout(queue_timeout, pool.acquire()).await {
        Ok(Ok(lease)) => lease,
        Ok(Err(error)) => {
            tracing::warn!(%error, "no worker connection for a gRPC call");
            let status = Status::new(
                code::UNAVAILABLE,
                "No worker is available to take the call.",
            );
            return recorded(recording, trailers_only(protocol, &status));
        }
        Err(_) if deadline.is_some_and(|at| Instant::now() >= at) => {
            let status = Status::new(code::DEADLINE_EXCEEDED, "Deadline exceeded");
            return recorded(recording, trailers_only(protocol, &status));
        }
        Err(_) => {
            let status = Status::new(
                code::UNAVAILABLE,
                "Every worker stayed busy for the whole queue timeout.",
            );
            return recorded(recording, trailers_only(protocol, &status));
        }
    };

    let mut recording = recording;
    recording.record.worker = Some(lease.worker);
    recording.record.queue = queued.elapsed();
    let dispatched = Instant::now();
    let mut reader = lease.conn.reader;
    let mut writer = lease.conn.writer;

    if let Err(error) =
        frame::write(&mut writer, kind::REQUEST, 0, head.encode_to_vec().into()).await
    {
        tracing::warn!(%error, "writing a gRPC call's head failed");
        let status = Status::new(code::UNAVAILABLE, "The worker is not available.");
        return recorded(recording, trailers_only(protocol, &status));
    }

    let back = Return::new(
        lease.woken.unwrap_or_else(|| Arc::clone(&pool)),
        lease.worker,
        lease.permit,
    );
    let failure = Arc::new(Mutex::new(None::<Status>));

    tokio::spawn(pump_messages(
        body,
        writer,
        Arc::clone(&back),
        Decoder::new(protocol, encoding, limits.max_body_size),
        Arc::clone(&failure),
    ));

    // The worker answers with its head, the initial metadata, or fails
    // before it.

    let far = Instant::now() + Duration::from_secs(365 * 24 * 3600);

    let first =
        tokio::time::timeout_at(deadline.unwrap_or(far).into(), frame::read(&mut reader)).await;

    let response_head = match first {
        Err(_) => {
            back.give_reader(None);
            back.abort_request();
            let status = Status::new(code::DEADLINE_EXCEEDED, "Deadline exceeded");
            return recorded(recording, trailers_only(protocol, &status));
        }
        Ok(Ok(Some(next))) if next.kind == kind::RESPONSE => {
            pb::ResponseHead::decode(next.payload).unwrap_or_default()
        }
        Ok(Ok(Some(next))) if next.kind == kind::END => {
            back.give_reader(Some(reader));
            back.abort_request();
            let end = pb::End::decode(next.payload).unwrap_or_default();
            let status = taken(&failure).unwrap_or_else(|| ended(&end));
            return recorded(recording, trailers_only(protocol, &status));
        }
        _ => {
            back.give_reader(None);
            back.abort_request();
            let status = Status::new(code::UNAVAILABLE, "The worker failed.");
            return recorded(recording, trailers_only(protocol, &status));
        }
    };

    recording.record.application = dispatched.elapsed();
    let mut response = Response::new(
        Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    crate::copy_headers(headers, response_head.headers);
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(protocol.content_type()),
    );

    let state = Stream {
        reader: Some(reader),
        back,
        protocol,
        deadline,
        idle: streaming.then_some(limits.response_idle_timeout),
        recording: Some(recording),
        bytes: 0,
        failure,
    };
    let stream = futures::stream::unfold(state, next_frame);
    *response.body_mut() = UnsyncBoxBody::new(StreamBody::new(stream));
    response
}

/// Log a response the call ended with before any worker answered.
fn recorded(recording: Recording, response: Response<ResponseBody>) -> Response<ResponseBody> {
    recording.complete(response.status().as_u16(), "", 0);
    response
}

/// A failure the request side found, which takes the place of the
/// worker's status.
fn taken(failure: &Mutex<Option<Status>>) -> Option<Status> {
    failure.lock().expect("failure poisoned").take()
}

/// The status of a call the worker ended with an END frame: its trailers,
/// or the failure it reports.
fn ended(end: &pb::End) -> Status {
    if end.reason == "soft_timeout" {
        return Status::new(code::DEADLINE_EXCEEDED, end.error.clone());
    }

    if !end.error.is_empty() {
        return Status::new(code::UNKNOWN, end.error.clone());
    }

    Status::new(code::UNKNOWN, "The worker ended the call without a status.")
}

/// A response that is only headers: the status of a call that ends
/// before it begins.
fn trailers_only(protocol: Protocol, status: &Status) -> Response<ResponseBody> {
    let mut response = Response::new(
        Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(protocol.content_type()),
    );

    for (name, value) in status.trailers() {
        headers.insert(name, value);
    }

    response
}

/// The response side of a call in progress.
struct Stream {
    reader: Option<Reader>,
    back: Arc<Return>,
    protocol: Protocol,
    deadline: Option<Instant>,
    /// For a streaming call, how long the worker may send nothing.
    idle: Option<Duration>,
    recording: Option<Recording>,
    bytes: u64,
    failure: Arc<Mutex<Option<Status>>>,
}

impl Stream {
    /// End the call: give the connection back if it is still good, stop
    /// the request side, log the call, and send the status.
    fn finish(
        &mut self,
        reader: Option<Reader>,
        status: Status,
        trailers: Vec<(HeaderName, HeaderValue)>,
    ) -> Frame<Bytes> {
        self.reader = None;
        self.back.give_reader(reader);
        self.back.abort_request();

        if let Some(recording) = self.recording.take() {
            recording.complete(200, "", self.bytes);
        }

        let trailers = if trailers.is_empty() {
            status.trailers()
        } else {
            trailers
        };

        if self.protocol.is_web() {
            let mut block = Vec::new();

            for (name, value) in &trailers {
                block.extend_from_slice(name.as_str().as_bytes());
                block.extend_from_slice(b": ");
                block.extend_from_slice(value.as_bytes());
                block.extend_from_slice(b"\r\n");
            }

            return Frame::data(self.framed(TRAILER_FRAME, &block));
        }

        let mut map = HeaderMap::new();

        for (name, value) in trailers {
            map.append(name, value);
        }

        Frame::trailers(map)
    }

    /// One message in gRPC framing, base64 encoded for grpc-web-text.
    fn framed(&self, flags: u8, data: &[u8]) -> Bytes {
        let mut framed = BytesMut::with_capacity(5 + data.len());
        framed.put_u8(flags);
        framed.put_u32(data.len() as u32);
        framed.put_slice(data);

        if self.protocol == Protocol::WebText {
            return Bytes::from(BASE64.encode(&framed));
        }

        framed.freeze()
    }
}

async fn next_frame(mut state: Stream) -> Option<(Result<Frame<Bytes>, BoxError>, Stream)> {
    let mut reader = state.reader.take()?;
    let far = Instant::now() + Duration::from_secs(365 * 24 * 3600);
    let mut until = state.deadline.unwrap_or(far);
    let mut idle_out = false;

    if let Some(idle) = state.idle
        && Instant::now() + idle < until
    {
        until = Instant::now() + idle;
        idle_out = true;
    }

    let next = tokio::time::timeout_at(until.into(), frame::read(&mut reader)).await;

    let frame = match next {
        Ok(Ok(Some(next))) if next.kind == kind::MESSAGE => {
            state.bytes += next.payload.len() as u64;
            let data = state.framed(0, &next.payload);
            state.reader = Some(reader);
            Frame::data(data)
        }
        Ok(Ok(Some(next))) if next.kind == kind::END => {
            let end = pb::End::decode(next.payload).unwrap_or_default();

            match taken(&state.failure) {
                Some(status) => state.finish(Some(reader), status, Vec::new()),
                None if end.trailers.is_empty() => {
                    let status = ended(&end);
                    state.finish(Some(reader), status, Vec::new())
                }
                None => {
                    let trailers = end
                        .trailers
                        .into_iter()
                        .filter_map(|header| {
                            Some((
                                HeaderName::from_bytes(&header.name).ok()?,
                                HeaderValue::from_bytes(&header.value).ok()?,
                            ))
                        })
                        .collect();
                    state.finish(Some(reader), Status::new(code::OK, ""), trailers)
                }
            }
        }
        Err(_) if idle_out => {
            let status = Status::new(
                code::DEADLINE_EXCEEDED,
                "The worker sent nothing for the idle timeout.",
            );
            state.finish(None, status, Vec::new())
        }
        Err(_) => {
            let status = Status::new(code::DEADLINE_EXCEEDED, "Deadline exceeded");
            state.finish(None, status, Vec::new())
        }
        _ => {
            let status = Status::new(code::UNAVAILABLE, "The worker failed during the call.");
            state.finish(None, status, Vec::new())
        }
    };

    Some((Ok(frame), state))
}

/// Send the request's messages to the worker as MESSAGE frames, then END.
/// A client that goes away, a malformed request, or the response ending
/// first sends ABORT instead.
async fn pump_messages(
    body: Option<Incoming>,
    mut writer: Writer,
    back: Arc<Return>,
    mut decoder: Decoder,
    failure: Arc<Mutex<Option<Status>>>,
) {
    let Some(mut body) = body else {
        finish_request(writer, &back).await;
        return;
    };

    loop {
        let next = tokio::select! {
            next = body.frame() => next,
            _ = back.abort.notified() => {
                abort_request(writer, &back).await;
                return;
            }
        };

        let data = match next {
            Some(Ok(next)) => match next.into_data() {
                Ok(data) => data,
                Err(_) => continue,
            },
            Some(Err(_)) => {
                abort_request(writer, &back).await;
                return;
            }
            None => break,
        };

        let messages = decoder.push(&data).and_then(|()| {
            let mut messages = Vec::new();

            while let Some(message) = decoder.next_message()? {
                messages.push(message);
            }

            Ok(messages)
        });

        let messages = match messages {
            Ok(messages) => messages,
            Err(status) => {
                *failure.lock().expect("failure poisoned") = Some(status);
                abort_request(writer, &back).await;
                return;
            }
        };

        for message in messages {
            if frame::write(&mut writer, kind::MESSAGE, 0, message)
                .await
                .is_err()
            {
                back.give_writer(None);
                return;
            }
        }
    }

    if !decoder.is_empty() {
        *failure.lock().expect("failure poisoned") = Some(Status::new(
            code::INTERNAL,
            "The request ended inside a message.",
        ));
        abort_request(writer, &back).await;
        return;
    }

    finish_request(writer, &back).await;
}

async fn finish_request(mut writer: Writer, back: &Return) {
    let end = pb::End::default().encode_to_vec();

    match frame::write(&mut writer, kind::END, 0, end.into()).await {
        Ok(()) => back.give_writer(Some(writer)),
        Err(_) => back.give_writer(None),
    }
}

async fn abort_request(mut writer: Writer, back: &Return) {
    let abort = pb::Abort::default().encode_to_vec();

    match frame::write(&mut writer, kind::ABORT, 0, abort.into()).await {
        Ok(()) => back.give_writer(Some(writer)),
        Err(_) => back.give_writer(None),
    }
}

/// Reads gRPC-framed messages out of a request body as it arrives.
struct Decoder {
    buffer: BytesMut,
    /// Base64 not yet decoded, for grpc-web-text.
    text: Option<BytesMut>,
    encoding: Encoding,
    max: u64,
}

impl Decoder {
    fn new(protocol: Protocol, encoding: Encoding, max: u64) -> Self {
        Self {
            buffer: BytesMut::new(),
            text: (protocol == Protocol::WebText).then(BytesMut::new),
            encoding,
            max,
        }
    }

    fn push(&mut self, data: &[u8]) -> Result<(), Status> {
        let Some(text) = self.text.as_mut() else {
            self.buffer.extend_from_slice(data);
            return Ok(());
        };

        text.extend(data.iter().filter(|byte| !byte.is_ascii_whitespace()));
        let usable = text.len() / 4 * 4;
        let decoded = BASE64
            .decode(&text[..usable])
            .map_err(|_| Status::new(code::INTERNAL, "The request is not valid base64."))?;
        text.advance(usable);
        self.buffer.extend_from_slice(&decoded);
        Ok(())
    }

    fn next_message(&mut self) -> Result<Option<Bytes>, Status> {
        if self.buffer.len() < 5 {
            return Ok(None);
        }

        let compressed = self.buffer[0] & 1 != 0;
        let length = u32::from_be_bytes([
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
            self.buffer[4],
        ]) as u64;

        if length > self.max {
            return Err(Status::new(
                code::RESOURCE_EXHAUSTED,
                format!(
                    "A request message of {length} bytes is over the limit of {} bytes.",
                    self.max
                ),
            ));
        }

        if (self.buffer.len() as u64) < 5 + length {
            return Ok(None);
        }

        self.buffer.advance(5);
        let data = self.buffer.split_to(length as usize).freeze();

        if !compressed {
            return Ok(Some(data));
        }

        let mut out = Vec::new();
        let limit = self.max + 1;

        let read = match self.encoding {
            Encoding::Identity => {
                return Err(Status::new(
                    code::INTERNAL,
                    "A compressed message arrived without a grpc-encoding.",
                ));
            }
            Encoding::Gzip => flate2::read::GzDecoder::new(data.reader())
                .take(limit)
                .read_to_end(&mut out),
            Encoding::Deflate => flate2::read::ZlibDecoder::new(data.reader())
                .take(limit)
                .read_to_end(&mut out),
        };

        if read.is_err() {
            return Err(Status::new(
                code::INTERNAL,
                "A request message could not be decompressed.",
            ));
        }

        if out.len() as u64 > self.max {
            return Err(Status::new(
                code::RESOURCE_EXHAUSTED,
                format!(
                    "A decompressed request message is over the limit of {} bytes.",
                    self.max
                ),
            ));
        }

        Ok(Some(Bytes::from(out)))
    }

    /// Whether nothing is left over: a body that ends with a partial
    /// message is malformed.
    fn is_empty(&self) -> bool {
        self.buffer.is_empty() && self.text.as_ref().is_none_or(BytesMut::is_empty)
    }
}

/// grpc.health.v1.HealthCheckRequest.
#[derive(Clone, PartialEq, prost::Message)]
struct HealthCheckRequest {
    #[prost(string, tag = "1")]
    service: String,
}

/// grpc.health.v1.HealthCheckResponse.
#[derive(Clone, PartialEq, prost::Message)]
struct HealthCheckResponse {
    #[prost(int32, tag = "1")]
    status: i32,
}

/// The health service's serving statuses.
const SERVING: i32 = 1;
const NOT_SERVING: i32 = 2;
const SERVICE_UNKNOWN: i32 = 3;

/// The standard health service (section 9.11): the release serves while
/// it has workers. The empty service name is the whole release; any
/// other must be a registered service. Watch sends the status, then
/// holds the call open until the client ends it.
async fn health(
    body: Option<Incoming>,
    pool: &LinkPool,
    protocol: Protocol,
    encoding: Encoding,
    watch: bool,
) -> Response<ResponseBody> {
    let data = match body {
        Some(mut body) => match crate::link::read_body(&mut body, 64 * 1024).await {
            Ok(data) => data,
            Err(_) => {
                let status = Status::new(code::INTERNAL, "The request could not be read.");
                return trailers_only(protocol, &status);
            }
        },
        None => Bytes::new(),
    };

    let mut decoder = Decoder::new(protocol, encoding, 64 * 1024);

    let request = match decoder
        .push(&data)
        .and_then(|()| decoder.next_message())
        .map(|message| HealthCheckRequest::decode(message.unwrap_or_default()))
    {
        Ok(Ok(request)) => request,
        Ok(Err(_)) => {
            let status = Status::new(code::INTERNAL, "The request is not a HealthCheckRequest.");
            return trailers_only(protocol, &status);
        }
        Err(status) => return trailers_only(protocol, &status),
    };

    let serving = if pool.is_empty() {
        NOT_SERVING
    } else {
        SERVING
    };

    let status = if request.service.is_empty() || pool.grpc_service(&request.service) {
        serving
    } else if watch {
        SERVICE_UNKNOWN
    } else {
        let status = Status::new(
            code::NOT_FOUND,
            format!("unknown service {}", request.service),
        );
        return trailers_only(protocol, &status);
    };

    let message = HealthCheckResponse { status }.encode_to_vec();
    let mut framed = BytesMut::with_capacity(5 + message.len());
    framed.put_u8(0);
    framed.put_u32(message.len() as u32);
    framed.put_slice(&message);

    let data = if protocol == Protocol::WebText {
        Bytes::from(BASE64.encode(&framed))
    } else {
        framed.freeze()
    };

    let mut response = Response::new(
        Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(protocol.content_type()),
    );

    // Watch never ends on its own: the status cannot change for this
    // set of workers, which a new deploy replaces.

    let ok = Status::new(code::OK, "");
    let mut frames: Vec<Frame<Bytes>> = vec![Frame::data(data)];

    if !watch {
        frames.push(end_frame(protocol, &ok));
    }

    let body = futures::stream::iter(frames.into_iter().map(Ok::<_, BoxError>));

    let body: ResponseBody = if watch {
        UnsyncBoxBody::new(StreamBody::new(body.chain(futures::stream::pending())))
    } else {
        UnsyncBoxBody::new(StreamBody::new(body))
    };
    *response.body_mut() = body;
    response
}

/// The status at the end of a call's body: trailers, or for gRPC-Web a
/// trailer frame.
fn end_frame(protocol: Protocol, status: &Status) -> Frame<Bytes> {
    if protocol.is_web() {
        let mut block = Vec::new();

        for (name, value) in status.trailers() {
            block.extend_from_slice(name.as_str().as_bytes());
            block.extend_from_slice(b": ");
            block.extend_from_slice(value.as_bytes());
            block.extend_from_slice(b"\r\n");
        }

        let mut framed = BytesMut::with_capacity(5 + block.len());
        framed.put_u8(TRAILER_FRAME);
        framed.put_u32(block.len() as u32);
        framed.put_slice(&block);

        if protocol == Protocol::WebText {
            return Frame::data(Bytes::from(BASE64.encode(&framed)));
        }

        return Frame::data(framed.freeze());
    }

    let mut map = HeaderMap::new();

    for (name, value) in status.trailers() {
        map.append(name, value);
    }

    Frame::trailers(map)
}

/// A grpc-timeout value: up to eight digits and a unit, H, M, S, m
/// (milliseconds), u (microseconds) or n (nanoseconds).
fn parse_timeout(text: &str) -> Option<Duration> {
    let text = text.trim();

    if text.len() < 2 || text.len() > 9 {
        return None;
    }

    let (digits, unit) = text.split_at(text.len() - 1);
    let amount: u64 = digits.parse().ok()?;

    Some(match unit {
        "H" => Duration::from_secs(amount * 3600),
        "M" => Duration::from_secs(amount * 60),
        "S" => Duration::from_secs(amount),
        "m" => Duration::from_millis(amount),
        "u" => Duration::from_micros(amount),
        "n" => Duration::from_nanos(amount),
        _ => return None,
    })
}

/// The grpc-message encoding: bytes outside printable ASCII, and %,
/// percent encoded.
fn percent_encode(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());

    for byte in text.bytes() {
        if (0x20..=0x7e).contains(&byte) && byte != b'%' {
            out.push(byte);
        } else {
            out.extend_from_slice(format!("%{byte:02X}").as_bytes());
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0];
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn content_types_name_the_protocol() {
        let of = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(value));
            Protocol::of(&headers)
        };

        assert_eq!(of("application/grpc"), Some(Protocol::Grpc));
        assert_eq!(of("application/grpc+proto"), Some(Protocol::Grpc));
        assert_eq!(of("application/grpc-web+proto"), Some(Protocol::Web));
        assert_eq!(of("application/grpc-web-text"), Some(Protocol::WebText));
        assert_eq!(of("application/json"), None);
    }

    #[test]
    fn messages_are_unframed_as_they_arrive() {
        let mut decoder = Decoder::new(Protocol::Grpc, Encoding::Identity, 100);
        let mut body = framed(b"one");
        body.extend(framed(b"two"));
        decoder.push(&body[..4]).unwrap();
        assert_eq!(decoder.next_message().unwrap(), None);
        decoder.push(&body[4..]).unwrap();
        assert_eq!(
            decoder.next_message().unwrap().as_deref(),
            Some(&b"one"[..])
        );
        assert_eq!(
            decoder.next_message().unwrap().as_deref(),
            Some(&b"two"[..])
        );
        assert!(decoder.is_empty());

        let mut small = Decoder::new(Protocol::Grpc, Encoding::Identity, 2);
        small.push(&framed(b"big")).unwrap();
        assert_eq!(
            small.next_message().unwrap_err().code,
            code::RESOURCE_EXHAUSTED
        );
    }

    #[test]
    fn web_text_and_compressed_messages_are_decoded() {
        let mut text = Decoder::new(Protocol::WebText, Encoding::Identity, 100);
        let encoded = BASE64.encode(framed(b"hello"));
        text.push(&encoded.as_bytes()[..5]).unwrap();
        text.push(&encoded.as_bytes()[5..]).unwrap();
        assert_eq!(text.next_message().unwrap().as_deref(), Some(&b"hello"[..]));

        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gzip, b"squeezed").unwrap();
        let mut body = framed(&gzip.finish().unwrap());
        body[0] = 1;
        let mut decoder = Decoder::new(Protocol::Grpc, Encoding::Gzip, 100);
        decoder.push(&body).unwrap();
        assert_eq!(
            decoder.next_message().unwrap().as_deref(),
            Some(&b"squeezed"[..])
        );
    }

    #[test]
    fn timeouts_and_messages_follow_the_wire_formats() {
        assert_eq!(parse_timeout("5S"), Some(Duration::from_secs(5)));
        assert_eq!(parse_timeout("250m"), Some(Duration::from_millis(250)));
        assert_eq!(parse_timeout("1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_timeout("123456789S"), None);
        assert_eq!(parse_timeout("5x"), None);
        assert_eq!(percent_encode("50% off\n"), b"50%25 off%0A");
    }

    #[test]
    fn grpc_web_answers_cors_for_the_origins_named() {
        let origins = WebOrigins::new(&["https://App.example.com/".to_owned()]);
        let request = |method: &str, origin: &str| {
            http::Request::builder()
                .method(method)
                .uri("/shop.Cart/Add")
                .header("origin", origin)
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "x-grpc-web, content-type")
                .body(())
                .unwrap()
                .into_parts()
                .0
        };

        let allowed = request("OPTIONS", "https://app.example.com");
        assert!(origins.is_preflight(&allowed));
        let answer = origins.preflight(&allowed);
        assert_eq!(answer.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            answer.headers()["access-control-allow-origin"],
            "https://app.example.com"
        );
        assert_eq!(
            answer.headers()["access-control-allow-headers"],
            "x-grpc-web, content-type"
        );
        assert_eq!(answer.headers()["access-control-allow-methods"], "POST");

        let other = request("OPTIONS", "https://evil.example.com");
        assert_eq!(origins.preflight(&other).status(), StatusCode::FORBIDDEN);
        assert!(!origins.is_preflight(&request("POST", "https://app.example.com")));
        assert!(!WebOrigins::new(&[]).is_preflight(&allowed));

        let mut response = Response::new(empty());
        origins.decorate(&allowed.headers, &mut response);
        assert_eq!(
            response.headers()["access-control-expose-headers"],
            WEB_EXPOSED
        );

        let mut response = Response::new(empty());
        origins.decorate(&other.headers, &mut response);
        assert!(response.headers().is_empty());

        let any = WebOrigins::new(&["*".to_owned()]);
        assert_eq!(any.preflight(&other).status(), StatusCode::NO_CONTENT);
    }
}
