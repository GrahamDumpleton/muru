//! Responses the edge generates itself: error pages and request identifiers
//! (design specification sections 4.12 and 4.19).
//!
//! Every generated response has a status and a reason from Appendix C.3,
//! carries the request identifier, and comes as HTML, an RFC 9457 problem
//! document or plain text according to what the client accepts. In
//! development mode an application error's traceback is included.
//!
//! Custom pages replace the built-in ones by status or status and reason
//! at any of four levels, most specific first: the route, the release,
//! the host and the node. A page is a file read when the snapshot is
//! compiled, a template rendered here, or an application path the edge
//! asks for the page as a sub-request, which [`crate::serve`] runs when
//! it finds a [`Handler`] on the response.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::convert::Infallible;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::{
    ACCEPT, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER,
};
use http::{HeaderMap, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use muru_proto::edge::v1 as pb;

use crate::ResponseBody;

/// The reason of a response the override replaced (section 4.12).
pub const OVERRIDE_REASON: &str = "error_override";

/// The response header an application sets to keep its own error
/// response when the override would replace it.
pub static X_MURU_ERROR_OVERRIDE: HeaderName = HeaderName::from_static("x-muru-error-override");

/// The largest page file read into memory.
const MAX_PAGE_BYTES: u64 = 1024 * 1024;

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
    /// Headers kept from the response the page replaces (section 4.12):
    /// Retry-After, WWW-Authenticate and Allow.
    pub headers: Vec<(HeaderName, HeaderValue)>,
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
            headers: Vec::new(),
        }
    }
}

/// What a custom page is (section 4.12).
#[derive(Debug)]
pub enum PageKind {
    /// A file's contents, read when the snapshot was compiled.
    File(Bytes),
    /// A template with `{status}`, `{title}`, `{reason}`, `{description}`,
    /// `{request_id}`, `{retry_after}`, `{host}` and `{path}`.
    Template(String),
    /// An application path asked for the page as a sub-request.
    Path(String),
}

/// A custom page for a status, or a status and reason.
#[derive(Debug)]
pub struct Page {
    /// The page.
    pub kind: PageKind,
    /// The content type a file or template is sent with.
    pub content_type: HeaderValue,
}

/// When a listed status of an application's own response is replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// Always.
    All,
    /// Only when the response has no body.
    EmptyBody,
    /// Only when the response is not JSON.
    NotJson,
    /// Only when the client accepts HTML.
    HtmlOnly,
}

/// The `error_override` of one level (section 4.12).
#[derive(Debug, Clone)]
pub struct Override {
    statuses: Vec<u16>,
    when: When,
}

/// The pages and override of one level: the node, a host, a release or
/// a route.
#[derive(Debug, Default)]
pub struct ErrorPolicy {
    pages: HashMap<String, Arc<Page>>,
    override_: Option<Override>,
}

impl ErrorPolicy {
    /// Compile one level's pages and override from the snapshot, reading
    /// each page file now. A file that cannot be read is reported.
    pub async fn load(
        pages: &[pb::ErrorPage],
        override_: Option<&pb::ErrorOverride>,
    ) -> Result<Self, String> {
        let mut compiled = HashMap::new();

        for page in pages {
            let (kind, content_type) = if !page.file.is_empty() {
                let path = Path::new(&page.file);
                let size = tokio::fs::metadata(path)
                    .await
                    .map_err(|error| format!("error page {}: {}: {error}", page.key, page.file))?
                    .len();

                if size > MAX_PAGE_BYTES {
                    return Err(format!(
                        "error page {}: {} is larger than {MAX_PAGE_BYTES} bytes",
                        page.key, page.file
                    ));
                }

                let content = tokio::fs::read(path)
                    .await
                    .map_err(|error| format!("error page {}: {}: {error}", page.key, page.file))?;
                (
                    PageKind::File(Bytes::from(content)),
                    crate::static_files::content_type(path),
                )
            } else if !page.template.is_empty() {
                let content_type = if page.content_type.is_empty() {
                    HeaderValue::from_static("text/html; charset=utf-8")
                } else {
                    HeaderValue::from_str(&page.content_type).map_err(|_| {
                        format!(
                            "error page {}: {:?} is not a content type",
                            page.key, page.content_type
                        )
                    })?
                };
                (PageKind::Template(page.template.clone()), content_type)
            } else if page.path.starts_with('/') {
                (
                    PageKind::Path(page.path.clone()),
                    HeaderValue::from_static("text/html; charset=utf-8"),
                )
            } else {
                return Err(format!(
                    "error page {}: a file, a template or a path is needed",
                    page.key
                ));
            };

            compiled.insert(page.key.clone(), Arc::new(Page { kind, content_type }));
        }

        let override_ = match override_ {
            Some(table) => Some(Override {
                statuses: table
                    .statuses
                    .iter()
                    .filter_map(|status| u16::try_from(*status).ok())
                    .collect(),
                when: match table.when.as_str() {
                    "" | "all" => When::All,
                    "empty_body" => When::EmptyBody,
                    "not_json" => When::NotJson,
                    "html_only" => When::HtmlOnly,
                    other => {
                        return Err(format!("error_override: {other:?} is not a known when"));
                    }
                },
            }),
            None => None,
        };

        Ok(Self {
            pages: compiled,
            override_,
        })
    }

    /// Whether the level says nothing.
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty() && self.override_.is_none()
    }

    /// The page for a status and reason at this level: the status:reason
    /// entry, else the status's.
    fn page(&self, status: u16, reason: &str) -> Option<Arc<Page>> {
        self.pages
            .get(&format!("{status}:{reason}"))
            .or_else(|| self.pages.get(&status.to_string()))
            .cloned()
    }
}

/// The levels in force for a request, most specific first.
#[derive(Debug, Clone, Default)]
pub struct Chain(Vec<Arc<ErrorPolicy>>);

impl Chain {
    /// The levels, most specific first; empty ones are left out.
    pub fn new(levels: Vec<Arc<ErrorPolicy>>) -> Self {
        Self(
            levels
                .into_iter()
                .filter(|level| !level.is_empty())
                .collect(),
        )
    }

    /// The page for a status and reason: the most specific level that
    /// has one for the status decides.
    pub fn page(&self, status: u16, reason: &str) -> Option<Arc<Page>> {
        self.0.iter().find_map(|level| level.page(status, reason))
    }

    /// Whether, and when, an application's response with this status is
    /// replaced: the most specific level with an override decides.
    pub fn override_for(&self, status: u16) -> Option<When> {
        let override_ = self.0.iter().find_map(|level| level.override_.as_ref())?;
        override_
            .statuses
            .contains(&status)
            .then_some(override_.when)
    }
}

/// An application path to ask for the page (section 4.12), left on a
/// generated response for [`crate::serve`] to act on: the response
/// itself is the built-in page, used if the handler fails.
#[derive(Debug, Clone)]
pub struct Handler {
    /// The application path.
    pub path: String,
    /// The status the page is for, which the handler's response takes.
    pub status: StatusCode,
    /// The reason, for `muru.error_reason`.
    pub reason: &'static str,
    /// Headers the final response keeps from the problem.
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

/// What a page is rendered with, beyond the problem.
pub struct Rendering<'a> {
    /// The request identifier.
    pub request_id: &'a str,
    /// What the client accepts, and whether tracebacks are shown.
    pub negotiation: &'a Negotiation,
    /// The custom pages in force.
    pub errors: &'a Chain,
    /// The host the client asked for, for templates.
    pub host: &'a str,
    /// The path the client asked for, for templates.
    pub path: &'a str,
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

/// Build the response for a problem: the custom page for its status and
/// reason when there is one, else the built-in page in the client's
/// format. A page that is an application path leaves a [`Handler`] on
/// the built-in response.
pub fn respond(problem: &Problem, rendering: &Rendering<'_>) -> Response<ResponseBody> {
    let title = problem.status.canonical_reason().unwrap_or("Error");
    let negotiation = rendering.negotiation;
    let detail = problem
        .detail
        .as_deref()
        .filter(|_| negotiation.development);
    let page = rendering
        .errors
        .page(problem.status.as_u16(), problem.reason);
    let mut handler = None;

    let (content_type, body) = match page.as_deref() {
        Some(Page {
            kind: PageKind::File(content),
            content_type,
        }) => (content_type.clone(), content.clone()),
        Some(Page {
            kind: PageKind::Template(template),
            content_type,
        }) => {
            let html = content_type
                .to_str()
                .is_ok_and(|value| value.starts_with("text/html"));
            let retry_after = problem
                .retry_after
                .map(|seconds| seconds.to_string())
                .unwrap_or_default();
            let values = [
                ("status", problem.status.as_u16().to_string()),
                ("title", title.to_owned()),
                ("reason", problem.reason.to_owned()),
                ("description", problem.description.clone()),
                ("request_id", rendering.request_id.to_owned()),
                ("retry_after", retry_after),
                ("host", rendering.host.to_owned()),
                ("path", rendering.path.to_owned()),
            ];
            (
                content_type.clone(),
                Bytes::from(render(template, html, &values)),
            )
        }
        Some(Page {
            kind: PageKind::Path(path),
            ..
        }) => {
            let mut headers = problem.headers.clone();

            if let Some(seconds) = problem.retry_after {
                headers.push((RETRY_AFTER, HeaderValue::from(seconds)));
            }

            handler = Some(Handler {
                path: path.clone(),
                status: problem.status,
                reason: problem.reason,
                headers,
            });
            built_in(
                problem,
                title,
                rendering.request_id,
                negotiation.format,
                detail,
            )
        }
        None => built_in(
            problem,
            title,
            rendering.request_id,
            negotiation.format,
            detail,
        ),
    };

    let length = body.len();
    let body = Full::new(body)
        .map_err(|never: Infallible| match never {})
        .boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = problem.status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, content_type);
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));

    if let Some(seconds) = problem.retry_after {
        headers.insert(RETRY_AFTER, HeaderValue::from(seconds));
    }

    for (name, value) in &problem.headers {
        headers.insert(name.clone(), value.clone());
    }

    if let Some(handler) = handler {
        response.extensions_mut().insert(handler);
    }

    response
}

/// The built-in page in the client's format.
fn built_in(
    problem: &Problem,
    title: &str,
    request_id: &str,
    format: Format,
    detail: Option<&str>,
) -> (HeaderValue, Bytes) {
    let (content_type, body) = match format {
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

    (HeaderValue::from_static(content_type), Bytes::from(body))
}

/// Fill a template's `{name}` placeholders from `values`, escaping them
/// for HTML when the page is HTML; anything else is left as written.
fn render(template: &str, html: bool, values: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];

        match after.find('}') {
            Some(end) if values.iter().any(|(name, _)| *name == &after[..end]) => {
                let name = &after[..end];
                let value = values
                    .iter()
                    .find(|(candidate, _)| *candidate == name)
                    .map(|(_, value)| value.as_str())
                    .unwrap_or_default();

                if html {
                    out.push_str(&escape(value));
                } else {
                    out.push_str(value);
                }

                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }

    out.push_str(rest);
    out
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
    fn templates_fill_their_placeholders_and_escape_html() {
        let values = [("status", "404".to_owned()), ("path", "/a<b>".to_owned())];
        assert_eq!(
            render("{status} at {path} {unknown} {", true, &values),
            "404 at /a&lt;b&gt; {unknown} {"
        );
        assert_eq!(render("{path}", false, &values), "/a<b>");
    }

    #[tokio::test]
    async fn the_most_specific_level_with_a_page_for_the_status_wins() {
        async fn level(pages: Vec<(&str, &str)>) -> Arc<ErrorPolicy> {
            let pages: Vec<pb::ErrorPage> = pages
                .into_iter()
                .map(|(key, template)| pb::ErrorPage {
                    key: key.to_owned(),
                    template: template.to_owned(),
                    ..Default::default()
                })
                .collect();
            Arc::new(ErrorPolicy::load(&pages, None).await.unwrap())
        }
        let route = level(vec![("500", "route")]).await;
        let host = level(vec![("404:no_route", "host"), ("404", "host-any")]).await;
        let node = level(vec![("404:internal_only", "node")]).await;
        let chain = Chain::new(vec![route, Arc::new(ErrorPolicy::default()), host, node]);
        let text = |page: Option<Arc<Page>>| match page.as_deref() {
            Some(Page {
                kind: PageKind::Template(template),
                ..
            }) => template.clone(),
            _ => String::new(),
        };
        assert_eq!(text(chain.page(500, "app_error")), "route");
        assert_eq!(text(chain.page(404, "no_route")), "host");
        assert_eq!(text(chain.page(404, "internal_only")), "host-any");
        assert_eq!(text(chain.page(503, "no_workers")), "");
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
