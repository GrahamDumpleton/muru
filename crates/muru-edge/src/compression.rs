//! Response compression (design specification section 4.17): the edge
//! compresses responses on behalf of applications, chosen against the
//! client's Accept-Encoding from the configured preference order, only
//! for the content types listed, for bodies at least `min_size` long or
//! of unknown length, and never for a response already encoded or one
//! the application opted out of. Streamed responses are compressed chunk
//! by chunk with a flush after each, so event streams keep flowing.

use std::io::Write as _;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, VARY,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
use http_body::{Body, Frame};
use http_body_util::combinators::UnsyncBoxBody;
use muru_proto::edge::v1 as pb;

use crate::{BoxError, ResponseBody};

/// The header an application sets to keep a response uncompressed.
pub static X_MURU_COMPRESS: HeaderName = HeaderName::from_static("x-muru-compress");

/// An encoding the edge can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Brotli.
    Br,
    /// Zstandard.
    Zstd,
    /// Gzip.
    Gzip,
}

impl Encoding {
    /// The token as Accept-Encoding and Content-Encoding write it.
    pub fn token(self) -> &'static str {
        match self {
            Self::Br => "br",
            Self::Zstd => "zstd",
            Self::Gzip => "gzip",
        }
    }

    /// Parse a token.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "br" => Some(Self::Br),
            "zstd" => Some(Self::Zstd),
            "gzip" | "x-gzip" => Some(Self::Gzip),
            _ => None,
        }
    }
}

/// How hard to compress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    /// Quickly.
    Fast,
    /// The usual balance.
    #[default]
    Default,
    /// Smallest output, at a cost.
    Best,
}

impl Level {
    /// Parse the setting's word.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "fast" => Some(Self::Fast),
            "default" => Some(Self::Default),
            "best" => Some(Self::Best),
            _ => None,
        }
    }

    /// The setting's word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Default => "default",
            Self::Best => "best",
        }
    }
}

/// The compression policy in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compression {
    /// Whether responses are compressed at all.
    pub enabled: bool,
    /// The encodings offered, in preference order.
    pub encodings: Vec<Encoding>,
    /// The smallest body worth compressing, in bytes.
    pub min_size: u64,
    /// The content types compressed: exact types, or `text/*`.
    pub types: Vec<String>,
    /// How hard to compress.
    pub level: Level,
}

impl Default for Compression {
    fn default() -> Self {
        Self {
            enabled: true,
            encodings: vec![Encoding::Br, Encoding::Zstd, Encoding::Gzip],
            min_size: 1024,
            types: [
                "text/*",
                "application/json",
                "application/javascript",
                "application/xml",
                "image/svg+xml",
            ]
            .iter()
            .map(|t| (*t).to_owned())
            .collect(),
            level: Level::Default,
        }
    }
}

impl Compression {
    /// A policy that compresses nothing.
    pub fn off() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// The policy a snapshot carries, checked: an unknown encoding or
    /// level is refused, so a daemon can reject the table before the
    /// edge takes it.
    pub fn from_snapshot(message: &pb::Compression) -> Result<Self, String> {
        let defaults = Self::default();
        let mut encodings = Vec::new();

        for token in &message.encodings {
            let encoding = Encoding::parse(token)
                .ok_or_else(|| format!("{token:?} is not an encoding: br, zstd or gzip"))?;

            if !encodings.contains(&encoding) {
                encodings.push(encoding);
            }
        }

        let level = if message.level.is_empty() {
            Level::Default
        } else {
            Level::parse(&message.level).ok_or_else(|| {
                format!("{:?} is not a level: fast, default or best", message.level)
            })?
        };

        Ok(Self {
            enabled: message.enabled,
            encodings: if message.encodings.is_empty() {
                defaults.encodings
            } else {
                encodings
            },
            min_size: if message.min_size == 0 {
                defaults.min_size
            } else {
                message.min_size
            },
            types: if message.types.is_empty() {
                defaults.types
            } else {
                message
                    .types
                    .iter()
                    .map(|t| t.trim().to_ascii_lowercase())
                    .collect()
            },
            level,
        })
    }

    /// The policy as a snapshot carries it.
    pub fn to_message(&self) -> pb::Compression {
        pb::Compression {
            enabled: self.enabled,
            encodings: self
                .encodings
                .iter()
                .map(|encoding| encoding.token().to_owned())
                .collect(),
            min_size: self.min_size,
            types: self.types.clone(),
            level: self.level.as_str().to_owned(),
        }
    }

    /// The encoding to use for a client, from what it accepts: the first
    /// of the policy's in order that the client takes, explicitly or
    /// through `*`.
    pub fn choose(&self, accept: Option<&HeaderValue>) -> Option<Encoding> {
        let accept = accept?.to_str().ok()?;
        let mut explicit: Vec<(String, f32)> = Vec::new();

        for part in accept.split(',') {
            let mut pieces = part.split(';');
            let token = pieces.next().unwrap_or("").trim().to_ascii_lowercase();

            if token.is_empty() {
                continue;
            }

            let quality = pieces
                .filter_map(|piece| piece.trim().strip_prefix("q=").map(str::trim))
                .find_map(|value| value.parse::<f32>().ok())
                .unwrap_or(1.0);
            explicit.push((token, quality));
        }

        let wildcard = explicit
            .iter()
            .find(|(token, _)| token == "*")
            .map(|(_, quality)| *quality);

        self.encodings.iter().copied().find(|encoding| {
            let named = explicit
                .iter()
                .find(|(token, _)| Encoding::parse(token) == Some(*encoding))
                .map(|(_, quality)| *quality);

            match (named, wildcard) {
                (Some(quality), _) => quality > 0.0,
                (None, Some(quality)) => quality > 0.0,
                (None, None) => false,
            }
        })
    }

    /// Whether a content type is one the policy compresses.
    pub fn compresses_type(&self, content_type: &str) -> bool {
        let essence = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        self.types
            .iter()
            .any(|wanted| match wanted.strip_suffix("/*") {
                Some(prefix) => essence
                    .split_once('/')
                    .is_some_and(|(kind, _)| kind == prefix),
                None => essence == *wanted,
            })
    }
}

/// Apply the policy to a response: compress it for the client when
/// everything says yes, and in any case strip the opt-out header.
pub fn apply(
    policy: &Compression,
    method: &Method,
    accept: Option<&HeaderValue>,
    mut response: Response<ResponseBody>,
) -> Response<ResponseBody> {
    let opted_out = response
        .headers_mut()
        .remove(&X_MURU_COMPRESS)
        .is_some_and(|value| {
            value
                .to_str()
                .map(|text| text.trim().eq_ignore_ascii_case("off"))
                .unwrap_or(false)
        });

    if !policy.enabled || opted_out || method == Method::HEAD {
        return response;
    }

    if !eligible(policy, response.status(), response.headers()) {
        return response;
    }

    let Some(encoding) = policy.choose(accept) else {
        return response;
    };

    let encoder = match Encoder::new(encoding, policy.level) {
        Ok(encoder) => encoder,
        Err(error) => {
            tracing::warn!(%error, "compression could not start");
            return response;
        }
    };

    let (mut parts, body) = response.into_parts();
    parts.headers.remove(CONTENT_LENGTH);
    parts
        .headers
        .insert(CONTENT_ENCODING, HeaderValue::from_static(encoding.token()));
    add_vary(&mut parts.headers);
    Response::from_parts(
        parts,
        UnsyncBoxBody::new(Compressed {
            inner: body,
            encoder: Some(encoder),
            trailers: None,
            done: false,
        }),
    )
}

/// Whether a response is one the policy may compress.
fn eligible(policy: &Compression, status: StatusCode, headers: &HeaderMap) -> bool {
    if status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
        || status == StatusCode::SWITCHING_PROTOCOLS
    {
        return false;
    }

    if headers.contains_key(CONTENT_ENCODING) {
        return false;
    }

    let no_transform = headers.get_all(CACHE_CONTROL).iter().any(|value| {
        value
            .to_str()
            .map(|text| {
                text.split(',')
                    .any(|directive| directive.trim().eq_ignore_ascii_case("no-transform"))
            })
            .unwrap_or(false)
    });

    if no_transform {
        return false;
    }

    let Some(content_type) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    if !policy.compresses_type(content_type) {
        return false;
    }

    let length = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse::<u64>().ok());

    length.is_none_or(|length| length >= policy.min_size)
}

/// Add Accept-Encoding to Vary, keeping what is there.
fn add_vary(headers: &mut HeaderMap) {
    let existing: Vec<String> = headers
        .get_all(VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();

    if existing.iter().any(|value| {
        value
            .split(',')
            .any(|item| item.trim() == "*" || item.trim().eq_ignore_ascii_case("accept-encoding"))
    }) {
        return;
    }

    headers.append(VARY, HeaderValue::from_static("Accept-Encoding"));
}

/// One encoder writing into a buffer that is taken after each flush.
enum Encoder {
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
    Brotli(Box<brotli::CompressorWriter<Vec<u8>>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}

impl Encoder {
    fn new(encoding: Encoding, level: Level) -> std::io::Result<Self> {
        Ok(match encoding {
            Encoding::Gzip => Self::Gzip(flate2::write::GzEncoder::new(
                Vec::new(),
                match level {
                    Level::Fast => flate2::Compression::fast(),
                    Level::Default => flate2::Compression::default(),
                    Level::Best => flate2::Compression::best(),
                },
            )),
            Encoding::Br => Self::Brotli(Box::new(brotli::CompressorWriter::new(
                Vec::new(),
                4096,
                match level {
                    Level::Fast => 3,
                    Level::Default => 5,
                    Level::Best => 11,
                },
                22,
            ))),
            Encoding::Zstd => Self::Zstd(zstd::stream::write::Encoder::new(
                Vec::new(),
                match level {
                    Level::Fast => 1,
                    Level::Default => 3,
                    Level::Best => 19,
                },
            )?),
        })
    }

    /// Compress a chunk and flush, returning what came out.
    fn write(&mut self, data: &[u8]) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Gzip(encoder) => {
                encoder.write_all(data)?;
                encoder.flush()?;
                Ok(std::mem::take(encoder.get_mut()))
            }
            Self::Brotli(encoder) => {
                encoder.write_all(data)?;
                encoder.flush()?;
                Ok(std::mem::take(encoder.get_mut()))
            }
            Self::Zstd(encoder) => {
                encoder.write_all(data)?;
                encoder.flush()?;
                Ok(std::mem::take(encoder.get_mut()))
            }
        }
    }

    /// Finish the stream, returning the last bytes.
    fn finish(self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Gzip(encoder) => encoder.finish(),
            Self::Brotli(encoder) => Ok(encoder.into_inner()),
            Self::Zstd(encoder) => encoder.finish(),
        }
    }
}

/// A body compressed as it streams.
struct Compressed {
    inner: ResponseBody,
    encoder: Option<Encoder>,
    trailers: Option<Frame<Bytes>>,
    done: bool,
}

impl Body for Compressed {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        loop {
            if let Some(trailers) = self.trailers.take() {
                return Poll::Ready(Some(Ok(trailers)));
            }

            if self.done {
                return Poll::Ready(None);
            }

            let next = match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(next) => next,
            };

            match next {
                Some(Ok(frame)) => {
                    if frame.is_data() {
                        let data = frame.into_data().unwrap_or_default();
                        let Some(encoder) = self.encoder.as_mut() else {
                            return Poll::Ready(None);
                        };

                        match encoder.write(&data) {
                            Ok(out) if out.is_empty() => continue,
                            Ok(out) => return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out))))),
                            Err(error) => return Poll::Ready(Some(Err(error.into()))),
                        }
                    }

                    // Trailers come after the compressed stream's end.

                    self.trailers = Some(frame);
                    self.done = true;

                    match self.encoder.take().map(Encoder::finish) {
                        Some(Ok(out)) if !out.is_empty() => {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                        }
                        Some(Err(error)) => return Poll::Ready(Some(Err(error.into()))),
                        _ => continue,
                    }
                }
                Some(Err(error)) => return Poll::Ready(Some(Err(error))),
                None => {
                    self.done = true;

                    match self.encoder.take().map(Encoder::finish) {
                        Some(Ok(out)) if !out.is_empty() => {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                        }
                        Some(Err(error)) => return Poll::Ready(Some(Err(error.into()))),
                        _ => return Poll::Ready(None),
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done && self.trailers.is_none()
    }
}

/// Whether a request's Accept-Encoding allows anything at all, for
/// callers that want to skip work early.
pub fn accepts_anything(headers: &HeaderMap) -> bool {
    headers.contains_key(ACCEPT_ENCODING)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};
    use std::io::Read as _;

    fn value(text: &str) -> HeaderValue {
        HeaderValue::from_str(text).unwrap()
    }

    #[test]
    fn the_first_preferred_encoding_the_client_takes_wins() {
        let policy = Compression::default();
        assert_eq!(
            policy.choose(Some(&value("gzip, deflate"))),
            Some(Encoding::Gzip)
        );
        assert_eq!(policy.choose(Some(&value("gzip, br"))), Some(Encoding::Br));
        assert_eq!(
            policy.choose(Some(&value("zstd;q=0.5, gzip;q=1"))),
            Some(Encoding::Zstd)
        );
        assert_eq!(policy.choose(Some(&value("*"))), Some(Encoding::Br));
        assert_eq!(
            policy.choose(Some(&value("br;q=0, *"))),
            Some(Encoding::Zstd)
        );
        assert_eq!(policy.choose(Some(&value("identity"))), None);
        assert_eq!(policy.choose(None), None);
    }

    #[test]
    fn types_match_exactly_or_by_family() {
        let policy = Compression::default();
        assert!(policy.compresses_type("text/html; charset=utf-8"));
        assert!(policy.compresses_type("application/json"));
        assert!(!policy.compresses_type("application/octet-stream"));
        assert!(!policy.compresses_type("image/png"));
    }

    #[test]
    fn a_snapshot_with_bad_words_is_refused() {
        let message = pb::Compression {
            enabled: true,
            encodings: vec!["gzip".into(), "lzma".into()],
            ..Default::default()
        };
        assert!(Compression::from_snapshot(&message).is_err());
        let message = pb::Compression {
            level: "max".into(),
            ..Default::default()
        };
        assert!(Compression::from_snapshot(&message).is_err());
    }

    #[tokio::test]
    async fn a_large_text_response_is_gzipped_and_a_small_one_is_not() {
        let policy = Compression::default();
        let large = "x".repeat(4096);
        let response = Response::builder()
            .header(CONTENT_TYPE, "text/plain")
            .header(CONTENT_LENGTH, large.len())
            .body(UnsyncBoxBody::new(
                Full::new(Bytes::from(large.clone())).map_err(|never| match never {}),
            ))
            .unwrap();
        let compressed = apply(&policy, &Method::GET, Some(&value("gzip")), response);
        assert_eq!(compressed.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
        assert_eq!(compressed.headers().get(VARY).unwrap(), "Accept-Encoding");
        assert!(compressed.headers().get(CONTENT_LENGTH).is_none());
        let body = compressed.into_body().collect().await.unwrap().to_bytes();
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(&body[..])
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, large);

        let small = Response::builder()
            .header(CONTENT_TYPE, "text/plain")
            .header(CONTENT_LENGTH, 5)
            .body(UnsyncBoxBody::new(
                Full::new(Bytes::from_static(b"hello")).map_err(|never| match never {}),
            ))
            .unwrap();
        let untouched = apply(&policy, &Method::GET, Some(&value("gzip")), small);
        assert!(untouched.headers().get(CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn an_opted_out_response_keeps_its_bytes_and_loses_the_header() {
        let policy = Compression::default();
        let response = Response::builder()
            .header(CONTENT_TYPE, "text/plain")
            .header(&X_MURU_COMPRESS, "off")
            .body(UnsyncBoxBody::new(
                Full::new(Bytes::from("x".repeat(4096))).map_err(|never| match never {}),
            ))
            .unwrap();
        let untouched = apply(&policy, &Method::GET, Some(&value("br")), response);
        assert!(untouched.headers().get(CONTENT_ENCODING).is_none());
        assert!(untouched.headers().get(&X_MURU_COMPRESS).is_none());
    }
}
