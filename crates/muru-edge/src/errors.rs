//! Responses the edge generates itself: error pages and request identifiers
//! (design specification sections 4.12 and 4.19).
//!
//! Every generated response has a status and a reason from Appendix C.3,
//! carries the request identifier, and comes as HTML, an RFC 9457 problem
//! document or plain text according to what the client accepts. In
//! development mode an application error's traceback is included.

use std::collections::hash_map::RandomState;
use std::convert::Infallible;
use std::hash::{BuildHasher, Hasher};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::{
    ACCEPT, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER,
};
use http::{HeaderMap, Response, StatusCode};
use http_body_util::{BodyExt, Full};

use crate::ResponseBody;

/// The response header carrying the request identifier.
pub static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// What a generated error response says.
pub struct Problem {
    /// The HTTP status.
    pub status: StatusCode,
    /// The reason from Appendix C.3, for example no_workers.
    pub reason: &'static str,
    /// One sentence for a person.
    pub description: String,
    /// A traceback or similar, shown only in development mode.
    pub detail: Option<String>,
    /// Seconds the client should wait before retrying, if any.
    pub retry_after: Option<u32>,
}

impl Problem {
    /// A problem with just a status, reason and description.
    pub fn new(status: StatusCode, reason: &'static str, description: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            description: description.into(),
            detail: None,
            retry_after: None,
        }
    }
}

/// What the response depends on from the request.
pub struct Negotiation {
    /// What the client accepts, from its Accept header.
    pub format: Format,
    /// Whether tracebacks are shown.
    pub development: bool,
}

impl Negotiation {
    /// Read the client's preference from its request headers.
    pub fn from_headers(headers: &HeaderMap, development: bool) -> Self {
        Self {
            format: Format::from_accept(headers.get(ACCEPT)),
            development,
        }
    }
}

/// The representation of an error page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// text/html, for browsers.
    Html,
    /// application/problem+json (RFC 9457), for API clients.
    Problem,
    /// text/plain, for everything else.
    Text,
}

impl Format {
    /// HTML when the client lists it, a problem document when it asks for
    /// JSON, plain text otherwise. Weights are not considered.
    fn from_accept(accept: Option<&HeaderValue>) -> Self {
        let Some(accept) = accept.and_then(|value| value.to_str().ok()) else {
            return Format::Text;
        };
        let mut wants_json = false;
        for item in accept.split(',') {
            let media = item.split(';').next().unwrap_or("").trim();
            if media.eq_ignore_ascii_case("text/html") {
                return Format::Html;
            }
            if media.eq_ignore_ascii_case("application/json")
                || media.eq_ignore_ascii_case("application/problem+json")
            {
                wants_json = true;
            }
        }
        if wants_json {
            Format::Problem
        } else {
            Format::Text
        }
    }
}

/// Build the response for a problem.
pub fn respond(
    problem: &Problem,
    request_id: &str,
    negotiation: &Negotiation,
) -> Response<ResponseBody> {
    let title = problem.status.canonical_reason().unwrap_or("Error");
    let detail = problem
        .detail
        .as_deref()
        .filter(|_| negotiation.development);
    let (content_type, body) = match negotiation.format {
        Format::Html => (
            "text/html; charset=utf-8",
            html(problem, title, request_id, detail),
        ),
        Format::Problem => (
            "application/problem+json",
            json(problem, title, request_id, detail),
        ),
        Format::Text => (
            "text/plain; charset=utf-8",
            text(problem, title, request_id, detail),
        ),
    };
    let length = body.len();
    let body = Full::new(Bytes::from(body))
        .map_err(|never: Infallible| match never {})
        .boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = problem.status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(seconds) = problem.retry_after {
        headers.insert(RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

fn text(problem: &Problem, title: &str, request_id: &str, detail: Option<&str>) -> String {
    let mut body = format!(
        "{} {}\nreason: {}\nrequest id: {}\n\n{}\n",
        problem.status.as_u16(),
        title,
        problem.reason,
        request_id,
        problem.description
    );
    if let Some(detail) = detail {
        body.push('\n');
        body.push_str(detail);
        if !detail.ends_with('\n') {
            body.push('\n');
        }
    }
    body
}

fn json(problem: &Problem, title: &str, request_id: &str, detail: Option<&str>) -> String {
    let mut body = format!(
        "{{\"type\":\"about:blank\",\"title\":{},\"status\":{},\"detail\":{},\"reason\":{},\"request_id\":{}",
        json_string(title),
        problem.status.as_u16(),
        json_string(&problem.description),
        json_string(problem.reason),
        json_string(request_id)
    );
    if let Some(detail) = detail {
        body.push_str(",\"traceback\":");
        body.push_str(&json_string(detail));
    }
    body.push_str("}\n");
    body
}

fn html(problem: &Problem, title: &str, request_id: &str, detail: Option<&str>) -> String {
    let status = problem.status.as_u16();
    let mut body = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{status} {title}</title>\n\
         <style>\n\
         body {{ margin: 0; padding: 3rem 1.5rem; font: 16px/1.5 system-ui, sans-serif; color: #1f2933; background: #f5f7fa; }}\n\
         main {{ max-width: 44rem; margin: 0 auto; }}\n\
         h1 {{ font-size: 1.75rem; margin: 0 0 1rem; }}\n\
         h1 span {{ color: #9aa5b1; font-weight: normal; margin-right: 0.5rem; }}\n\
         dl {{ display: grid; grid-template-columns: max-content 1fr; gap: 0.25rem 1rem; margin: 1.5rem 0; }}\n\
         dt {{ color: #616e7c; }}\n\
         pre {{ overflow: auto; padding: 1rem; background: #fff; border: 1px solid #d9e2ec; border-radius: 4px; font-size: 0.875rem; }}\n\
         footer {{ margin-top: 2rem; color: #9aa5b1; font-size: 0.875rem; }}\n\
         </style>\n</head>\n<body>\n<main>\n\
         <h1><span>{status}</span>{}</h1>\n<p>{}</p>\n\
         <dl><dt>Reason</dt><dd>{}</dd><dt>Request id</dt><dd>{}</dd></dl>\n",
        escape(title),
        escape(&problem.description),
        escape(problem.reason),
        escape(request_id)
    );
    if let Some(detail) = detail {
        body.push_str("<pre>");
        body.push_str(&escape(detail));
        body.push_str("</pre>\n");
    }
    body.push_str("<footer>Muru</footer>\n</main>\n</body>\n</html>\n");
    body
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(character),
        }
    }
    out
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Generates request identifiers: ULIDs, so they sort by time and never
/// repeat within a process, with the random part seeded once per process.
pub struct RequestIds {
    state: Mutex<IdState>,
}

struct IdState {
    last_millis: u64,
    random: u128,
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl Default for RequestIds {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestIds {
    /// Seed the random part from the process's own hash randomisation.
    pub fn new() -> Self {
        let mut random = 0u128;
        for _ in 0..2 {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u64(std::process::id().into());
            random = (random << 64) | u128::from(hasher.finish());
        }
        Self {
            state: Mutex::new(IdState {
                last_millis: 0,
                random: random & ((1u128 << 80) - 1),
            }),
        }
    }

    /// The next identifier: 26 characters of Crockford base32.
    pub fn next(&self) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
            & ((1u64 << 48) - 1);
        let mut state = self.state.lock().expect("request id state poisoned");
        if millis > state.last_millis {
            state.last_millis = millis;
        } else {
            // Same millisecond, or the clock stepped back: stay monotonic.
            state.random = (state.random + 1) & ((1u128 << 80) - 1);
        }
        let value = (u128::from(state.last_millis) << 80) | state.random;
        let mut out = [b'0'; 26];
        for (index, slot) in out.iter_mut().enumerate() {
            let shift = 125 - 5 * index as u32;
            *slot = CROCKFORD[((value >> shift) & 31) as usize];
        }
        String::from_utf8(out.to_vec()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_ordered() {
        let ids = RequestIds::new();
        let first = ids.next();
        let second = ids.next();
        assert_eq!(first.len(), 26);
        assert!(second > first);
        assert!(first.bytes().all(|byte| CROCKFORD.contains(&byte)));
    }

    #[test]
    fn format_follows_accept() {
        let header = |value: &'static str| HeaderValue::from_static(value);
        assert_eq!(Format::from_accept(None), Format::Text);
        assert_eq!(Format::from_accept(Some(&header("*/*"))), Format::Text);
        assert_eq!(
            Format::from_accept(Some(&header("text/html,application/xhtml+xml,*/*;q=0.8"))),
            Format::Html
        );
        assert_eq!(
            Format::from_accept(Some(&header("application/json"))),
            Format::Problem
        );
        assert_eq!(
            Format::from_accept(Some(&header("application/problem+json, text/html"))),
            Format::Html
        );
    }

    #[test]
    fn problem_document_is_json() {
        let problem = Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_workers",
            "No \"worker\".",
        );
        let body = json(
            &problem,
            "Service Unavailable",
            "01ARZ3",
            Some("line1\nline2"),
        );
        assert!(body.starts_with("{\"type\":\"about:blank\",\"title\":\"Service Unavailable\""));
        assert!(body.contains("\"detail\":\"No \\\"worker\\\".\""));
        assert!(body.contains("\"traceback\":\"line1\\nline2\""));
    }
}
