//! Static files (design specification section 4.9): a route that names a
//! directory is served by the edge, with ETag and Last-Modified,
//! conditional requests, single ranges, precompressed variants next to
//! the file, index files for directories, a redirect to the slash form of
//! a directory, cache headers, and a fallback file for single-page
//! applications. Hidden files are never served, paths are normalised and
//! checked against the canonical name on disk, and symbolic links that
//! leave the directory are refused.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use http::header::{
    ACCEPT_RANGES, ALLOW, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED, LOCATION, RANGE,
    SERVER, VARY,
};
use http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::body::Frame;
use muru_proto::edge::v1 as pb;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::ResponseBody;
use crate::compression::{Compression, Encoding};

/// How big each chunk of a file body is.
const CHUNK: usize = 64 * 1024;

/// A static route, as the snapshot describes it.
#[derive(Debug)]
pub struct StaticRoute {
    /// The directory served.
    pub directory: PathBuf,
    /// The files tried, in order, when a directory is requested.
    pub index: Vec<String>,
    /// The file served for any path that has no file, for single-page
    /// applications.
    pub fallback_file: Option<String>,
    /// Cache headers.
    pub cache: Cache,
    /// Whether a directory asked for without a trailing slash is
    /// redirected to the slash form.
    pub redirect_slash: bool,
    /// Whether a path may differ from the file's name on disk in case
    /// and Unicode normalisation.
    pub case_insensitive: bool,
}

/// The cache headers a static route sends.
#[derive(Debug, Default)]
pub struct Cache {
    /// `max-age`, when set.
    pub max_age: Option<u64>,
    /// Whether `immutable` is added.
    pub immutable: bool,
    /// Files that get `no-cache` instead, by pattern on their path within
    /// the route.
    pub except: Option<globset::GlobSet>,
}

impl StaticRoute {
    /// A route from a snapshot's route message, when it names a directory.
    pub fn from_message(route: &pb::Route) -> Result<Option<Self>, String> {
        if route.static_dir.is_empty() {
            return Ok(None);
        }

        let mut builder = globset::GlobSetBuilder::new();

        for pattern in &route.cache_except {
            builder.add(
                globset::Glob::new(pattern)
                    .map_err(|error| format!("cache.except {pattern:?}: {error}"))?,
            );
        }

        let except = if route.cache_except.is_empty() {
            None
        } else {
            Some(
                builder
                    .build()
                    .map_err(|error| format!("cache.except: {error}"))?,
            )
        };

        Ok(Some(Self {
            directory: PathBuf::from(&route.static_dir),
            index: if route.index.is_empty() {
                vec!["index.html".to_owned()]
            } else {
                route.index.clone()
            },
            fallback_file: (!route.fallback_file.is_empty()).then(|| route.fallback_file.clone()),
            cache: Cache {
                max_age: (route.cache_max_age_s > 0).then_some(route.cache_max_age_s),
                immutable: route.cache_immutable,
                except,
            },
            redirect_slash: !route.no_redirect_slash,
            case_insensitive: route.case_insensitive,
        }))
    }
}

/// What serving produced.
pub enum Outcome {
    /// A response, with what the access log records about it.
    Served {
        /// The response.
        response: Response<ResponseBody>,
        /// Its status.
        status: u16,
        /// The reason, for the access log.
        reason: &'static str,
        /// The body's length.
        bytes: u64,
    },
    /// No file answers; the route's fallback target takes the request,
    /// or it is refused.
    Fallback,
    /// The method is not one files answer to and nothing else takes it.
    MethodNotAllowed,
}

/// A file found for a request.
struct Found {
    path: PathBuf,
    relative: String,
    length: u64,
    modified: Option<SystemTime>,
}

/// Where a request's path led.
enum Resolved {
    File(Found),
    RedirectToSlash,
    Nothing,
}

/// Serve a request against a static route. `within` is the request path
/// below the route's prefix, `target` the whole path with its query for
/// a redirect, and `policy` says which precompressed variants a client
/// may be given.
pub async fn serve(
    route: &Arc<StaticRoute>,
    method: &Method,
    within: &str,
    target: &str,
    headers: &HeaderMap,
    policy: &Compression,
) -> Outcome {
    if method != Method::GET && method != Method::HEAD {
        return Outcome::MethodNotAllowed;
    }

    let trailing_slash = within.ends_with('/') || within.is_empty();

    // A hidden or climbing path has no file; it falls through like any
    // other path without one.

    let resolved = match request_components(within) {
        Some(components) => {
            let route_for_resolve = Arc::clone(route);
            tokio::task::spawn_blocking(move || {
                resolve(&route_for_resolve, &components, trailing_slash)
            })
            .await
            .unwrap_or(Resolved::Nothing)
        }
        None => Resolved::Nothing,
    };

    let found = match resolved {
        Resolved::File(found) => found,
        Resolved::RedirectToSlash => {
            let (path, query) = target.split_once('?').unwrap_or((target, ""));

            let location = if query.is_empty() {
                format!("{path}/")
            } else {
                format!("{path}/?{query}")
            };
            return redirect(&location);
        }
        Resolved::Nothing => {
            let Some(fallback) = &route.fallback_file else {
                return Outcome::Fallback;
            };
            let route_for_fallback = Arc::clone(route);
            let name = fallback.clone();
            let fallback = tokio::task::spawn_blocking(move || {
                let Some(components) = request_components(&name) else {
                    return Resolved::Nothing;
                };
                resolve(&route_for_fallback, &components, false)
            })
            .await
            .unwrap_or(Resolved::Nothing);

            match fallback {
                Resolved::File(found) => found,
                _ => return Outcome::Fallback,
            }
        }
    };

    respond(route, method, headers, found, policy).await
}

/// The path's components below the route, percent-decoded and
/// normalised, or none when it tries to climb out or names a hidden
/// file.
fn request_components(within: &str) -> Option<Vec<String>> {
    let decoded = percent_encoding::percent_decode_str(within)
        .decode_utf8()
        .ok()?;

    if decoded.contains('\0') {
        return None;
    }

    let mut components = Vec::new();

    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            hidden if hidden.starts_with('.') => return None,
            name => components.push(name.to_owned()),
        }
    }

    Some(components)
}

/// Find the file a request names, on the blocking pool: the path is
/// contained in the directory by canonical path, matched against the
/// name on disk, and a directory yields its index file or a redirect.
fn resolve(route: &StaticRoute, components: &[String], trailing_slash: bool) -> Resolved {
    let Ok(root) = std::fs::canonicalize(&route.directory) else {
        return Resolved::Nothing;
    };
    let mut candidate = root.clone();

    for component in components {
        candidate.push(component);
    }

    let Ok(canonical) = std::fs::canonicalize(&candidate) else {
        return Resolved::Nothing;
    };

    if !canonical.starts_with(&root) {
        tracing::warn!(path = %candidate.display(), "a static path leaves its directory and was refused");
        return Resolved::Nothing;
    }

    if !route.case_insensitive && !same_names(&root, components) {
        return Resolved::Nothing;
    }

    let Ok(metadata) = std::fs::metadata(&canonical) else {
        return Resolved::Nothing;
    };

    if metadata.is_dir() {
        if !trailing_slash && route.redirect_slash {
            return Resolved::RedirectToSlash;
        }

        for index in &route.index {
            let Some(parts) = request_components(index) else {
                continue;
            };
            let mut inner = components.to_vec();
            inner.extend(parts);

            if let Resolved::File(found) = resolve(route, &inner, false) {
                return Resolved::File(found);
            }
        }

        return Resolved::Nothing;
    }

    if !metadata.is_file() || trailing_slash {
        return Resolved::Nothing;
    }

    Resolved::File(Found {
        path: canonical,
        relative: components.join("/"),
        length: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// Whether each name in the request is an entry of its directory exactly
/// as written, in case and in form, which keeps a case-insensitive file
/// system from answering to names the request did not use. A symbolic
/// link is matched by its own name, not its target's.
fn same_names(root: &Path, components: &[String]) -> bool {
    let mut directory = root.to_path_buf();

    for name in components {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return false;
        };
        let wanted = std::ffi::OsStr::new(name.as_str());

        if !entries
            .flatten()
            .any(|entry| entry.file_name().as_os_str() == wanted)
        {
            return false;
        }

        directory.push(name);
    }

    true
}

/// The response for a file: conditional requests, ranges, precompressed
/// variants and cache headers.
async fn respond(
    route: &StaticRoute,
    method: &Method,
    headers: &HeaderMap,
    found: Found,
    policy: &Compression,
) -> Outcome {
    let etag = etag(&found);
    let modified = found.modified.map(httpdate::fmt_http_date);
    let mut base = HeaderMap::new();
    base.insert(SERVER, HeaderValue::from_static("muru"));
    base.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    if let Ok(value) = HeaderValue::from_str(&etag) {
        base.insert(ETAG, value);
    }

    if let Some(value) = modified
        .as_deref()
        .and_then(|text| HeaderValue::from_str(text).ok())
    {
        base.insert(LAST_MODIFIED, value);
    }

    if let Some(value) = cache_control(route, &found.relative) {
        base.insert(CACHE_CONTROL, value);
    }

    // Conditional requests (RFC 9110 section 13): a matching ETag or an
    // unchanged modification time is answered 304 without a body.

    if not_modified(headers, &etag, found.modified) {
        let mut response = Response::new(empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        *response.headers_mut() = base;
        return Outcome::Served {
            response,
            status: 304,
            reason: "static",
            bytes: 0,
        };
    }

    let content_type = content_type(&found.path);
    base.insert(CONTENT_TYPE, content_type);

    // A range request is answered from the file itself; otherwise a
    // precompressed variant next to the file is preferred when the
    // client takes its encoding.

    let range = headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|_| if_range_holds(headers, &etag, modified.as_deref()))
        .and_then(|value| parse_range(value, found.length));

    match range {
        Some(Range::Unsatisfiable) => {
            let mut response = Response::new(empty());
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            *response.headers_mut() = base;

            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{}", found.length)) {
                response.headers_mut().insert(CONTENT_RANGE, value);
            }

            response
                .headers_mut()
                .insert(CONTENT_LENGTH, HeaderValue::from(0));
            return Outcome::Served {
                response,
                status: 416,
                reason: "range_not_satisfiable",
                bytes: 0,
            };
        }
        Some(Range::Bytes(start, end)) => {
            let length = end - start + 1;

            let mut response = match body_of(&found.path, start, length, method).await {
                Ok(body) => Response::new(body),
                Err(error) => return unreadable(error),
            };

            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            *response.headers_mut() = base;

            if let Ok(value) =
                HeaderValue::from_str(&format!("bytes {start}-{end}/{}", found.length))
            {
                response.headers_mut().insert(CONTENT_RANGE, value);
            }

            response
                .headers_mut()
                .insert(CONTENT_LENGTH, HeaderValue::from(length));
            return Outcome::Served {
                response,
                status: 206,
                reason: "static",
                bytes: length,
            };
        }
        None => {}
    }

    let variant = precompressed(&found.path, headers, policy).await;

    let (path, length, encoding) = match &variant {
        Some((path, length, encoding)) => (path.clone(), *length, Some(*encoding)),
        None => (found.path.clone(), found.length, None),
    };

    let mut response = match body_of(&path, 0, length, method).await {
        Ok(body) => Response::new(body),
        Err(error) => return unreadable(error),
    };

    *response.headers_mut() = base;
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(length));

    if let Some(encoding) = encoding {
        response
            .headers_mut()
            .insert(CONTENT_ENCODING, HeaderValue::from_static(encoding.token()));
        response
            .headers_mut()
            .insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    }

    Outcome::Served {
        response,
        status: 200,
        reason: "static",
        bytes: length,
    }
}

/// A precompressed variant of the file (`.br`, `.zst` or `.gz`) that the
/// client accepts, in the policy's preference order, with its length.
async fn precompressed(
    path: &Path,
    headers: &HeaderMap,
    policy: &Compression,
) -> Option<(PathBuf, u64, Encoding)> {
    if !policy.enabled {
        return None;
    }

    for encoding in policy.accepted(headers.get(http::header::ACCEPT_ENCODING)) {
        let suffix = match encoding {
            Encoding::Br => "br",
            Encoding::Zstd => "zst",
            Encoding::Gzip => "gz",
        };

        let mut candidate = path.as_os_str().to_owned();
        candidate.push(".");
        candidate.push(suffix);
        let candidate = PathBuf::from(candidate);

        if let Ok(metadata) = tokio::fs::metadata(&candidate).await
            && metadata.is_file()
        {
            return Some((candidate, metadata.len(), encoding));
        }
    }

    None
}

/// The file's bytes from `start` for `length`, streamed in chunks, or an
/// empty body for HEAD.
async fn body_of(
    path: &Path,
    start: u64,
    length: u64,
    method: &Method,
) -> std::io::Result<ResponseBody> {
    if method == Method::HEAD {
        return Ok(empty());
    }

    let mut file = tokio::fs::File::open(path).await?;

    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }

    let stream = futures::stream::try_unfold((file, length), |(mut file, remaining)| async move {
        if remaining == 0 {
            return Ok(None);
        }

        let wanted = usize::try_from(remaining.min(CHUNK as u64)).unwrap_or(CHUNK);
        let mut buffer = vec![0u8; wanted];
        let read = file.read(&mut buffer).await?;

        if read == 0 {
            return Ok(None);
        }

        buffer.truncate(read);
        Ok(Some((
            Frame::data(Bytes::from(buffer)),
            (file, remaining - read as u64),
        )))
    });

    Ok(StreamBody::new(stream)
        .map_err(|error: std::io::Error| Box::new(error) as crate::BoxError)
        .boxed_unsync())
}

/// A file that was found but could not be opened: a 404 rather than a
/// 500, since to the client it is as good as absent.
fn unreadable(error: std::io::Error) -> Outcome {
    tracing::warn!(%error, "a static file could not be read");
    Outcome::Fallback
}

/// A redirect to the slash form of a directory.
fn redirect(location: &str) -> Outcome {
    let mut response = Response::new(empty());
    *response.status_mut() = StatusCode::MOVED_PERMANENTLY;
    let headers = response.headers_mut();

    if let Ok(value) = HeaderValue::from_str(location) {
        headers.insert(LOCATION, value);
    }

    headers.insert(CONTENT_LENGTH, HeaderValue::from(0));
    headers.insert(SERVER, HeaderValue::from_static("muru"));
    Outcome::Served {
        response,
        status: 301,
        reason: "redirect_slash",
        bytes: 0,
    }
}

/// The 405 for a method files do not answer.
pub fn method_not_allowed() -> Response<ResponseBody> {
    let mut response = Response::new(empty());
    *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    let headers = response.headers_mut();
    headers.insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(0));
    headers.insert(SERVER, HeaderValue::from_static("muru"));
    response
}

fn empty() -> ResponseBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// A strong validator from the file's length and modification time.
fn etag(found: &Found) -> String {
    let modified = found
        .modified
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    format!("\"{:x}-{:x}\"", found.length, modified)
}

/// Whether the request's validators say the client's copy is current.
fn not_modified(headers: &HeaderMap, etag: &str, modified: Option<SystemTime>) -> bool {
    if let Some(value) = headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    {
        return value.split(',').any(|candidate| {
            let candidate = candidate.trim();
            candidate == "*" || candidate.trim_start_matches("W/") == etag
        });
    }

    match (
        headers
            .get(IF_MODIFIED_SINCE)
            .and_then(|value| value.to_str().ok())
            .and_then(|text| httpdate::parse_http_date(text).ok()),
        modified,
    ) {
        (Some(since), Some(modified)) => {
            // HTTP dates have whole seconds.

            let modified = modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(d.as_secs()))
                .unwrap_or(modified);
            modified <= since
        }
        _ => false,
    }
}

/// Whether an If-Range precondition, when there is one, still holds, so
/// a range may be served rather than the whole file.
fn if_range_holds(headers: &HeaderMap, etag: &str, modified: Option<&str>) -> bool {
    let Some(value) = headers.get(IF_RANGE).and_then(|value| value.to_str().ok()) else {
        return true;
    };
    let value = value.trim();

    if value.starts_with('"') || value.starts_with("W/") {
        return value == etag;
    }

    modified == Some(value)
}

/// A single byte range of a file.
enum Range {
    Bytes(u64, u64),
    Unsatisfiable,
}

/// Parse `bytes=a-b`, `bytes=a-` or `bytes=-n`; several ranges are
/// served whole, as the whole file, by returning none.
fn parse_range(value: &str, length: u64) -> Option<Range> {
    let spec = value.trim().strip_prefix("bytes=")?;

    if spec.contains(',') || length == 0 {
        return if length == 0 {
            Some(Range::Unsatisfiable)
        } else {
            None
        };
    }

    let (start, end) = spec.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());

    if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;

        if suffix == 0 {
            return Some(Range::Unsatisfiable);
        }

        let start = length.saturating_sub(suffix);
        return Some(Range::Bytes(start, length - 1));
    }

    let start: u64 = start.parse().ok()?;

    if start >= length {
        return Some(Range::Unsatisfiable);
    }

    let end: u64 = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().ok()?.min(length - 1)
    };

    if end < start {
        return None;
    }

    Some(Range::Bytes(start, end))
}

/// The content type from the file's name, with a charset for text.
fn content_type(path: &Path) -> HeaderValue {
    let guess = mime_guess::from_path(path).first_or_octet_stream();

    let text = if guess.type_() == mime_guess::mime::TEXT {
        format!("{guess}; charset=utf-8")
    } else {
        guess.to_string()
    };

    HeaderValue::from_str(&text)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

/// The Cache-Control header for a file, from the route's cache table.
fn cache_control(route: &StaticRoute, relative: &str) -> Option<HeaderValue> {
    let max_age = route.cache.max_age?;

    if route
        .cache
        .except
        .as_ref()
        .is_some_and(|except| except.is_match(relative))
    {
        return Some(HeaderValue::from_static("no-cache"));
    }

    let text = if route.cache.immutable {
        format!("public, max-age={max_age}, immutable")
    } else {
        format!("public, max-age={max_age}")
    };

    HeaderValue::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalised_and_hidden_or_climbing_ones_refused() {
        assert_eq!(
            request_components("a/b/c.txt").unwrap(),
            vec!["a", "b", "c.txt"]
        );
        assert_eq!(request_components("a//./b/").unwrap(), vec!["a", "b"]);
        assert_eq!(
            request_components("caf%C3%A9.txt").unwrap(),
            vec!["café.txt"]
        );
        assert!(request_components("a/../b").is_none());
        assert!(request_components(".git/config").is_none());
        assert!(request_components("a/.hidden").is_none());
        assert!(request_components("a%00b").is_none());
    }

    #[test]
    fn ranges_follow_the_syntax() {
        assert!(matches!(
            parse_range("bytes=0-4", 100),
            Some(Range::Bytes(0, 4))
        ));
        assert!(matches!(
            parse_range("bytes=90-", 100),
            Some(Range::Bytes(90, 99))
        ));
        assert!(matches!(
            parse_range("bytes=-10", 100),
            Some(Range::Bytes(90, 99))
        ));
        assert!(matches!(
            parse_range("bytes=0-500", 100),
            Some(Range::Bytes(0, 99))
        ));
        assert!(matches!(
            parse_range("bytes=100-", 100),
            Some(Range::Unsatisfiable)
        ));
        assert!(parse_range("bytes=0-1,5-6", 100).is_none());
        assert!(parse_range("items=0-1", 100).is_none());
    }

    #[test]
    fn content_types_come_from_the_name() {
        assert_eq!(
            content_type(Path::new("a.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("a.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a.png")), "image/png");
        assert_eq!(content_type(Path::new("a")), "application/octet-stream");
    }
}
