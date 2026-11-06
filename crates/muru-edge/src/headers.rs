//! Headers a virtual host or a route removes from, sets on or adds to
//! the requests it takes and the responses to them (design specification
//! section 4.7).
//!
//! A table's names are removed first, then its headers are set, each
//! replacing any there, then those it adds are appended beside any there.
//! A host's table is applied before its route's, so the route has the
//! last word. Request rules are applied once to a client's own request,
//! after the edge's own removals and before anything is asked to serve
//! it; response rules to whatever answers it, the edge's own errors
//! included.

use http::{HeaderMap, HeaderName, HeaderValue};
use muru_proto::edge::v1 as pb;

/// One table's rules, compiled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    remove: Vec<HeaderName>,
    set: Vec<(HeaderName, HeaderValue)>,
    add: Vec<(HeaderName, HeaderValue)>,
}

impl Rules {
    /// The rules a snapshot carries; a name or value that is not one is
    /// reported.
    pub fn from_message(message: &pb::HeaderRules) -> Result<Self, String> {
        let name = |text: &str| {
            HeaderName::from_bytes(text.as_bytes())
                .map_err(|_| format!("{text:?} is not a header name"))
        };

        let field = |field: &pb::HeaderField| {
            let value = HeaderValue::from_str(&field.value)
                .map_err(|_| format!("{}: {:?} is not a header value", field.name, field.value))?;
            Ok::<_, String>((name(&field.name)?, value))
        };

        Ok(Self {
            remove: message
                .remove
                .iter()
                .map(|text| name(text))
                .collect::<Result<_, _>>()?,
            set: message.set.iter().map(field).collect::<Result<_, _>>()?,
            add: message.add.iter().map(field).collect::<Result<_, _>>()?,
        })
    }

    /// Whether the table does nothing.
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.set.is_empty() && self.add.is_empty()
    }

    /// Apply the rules to a message's headers.
    pub fn apply(&self, headers: &mut HeaderMap) {
        for name in &self.remove {
            headers.remove(name);
        }

        for (name, value) in &self.set {
            headers.insert(name.clone(), value.clone());
        }

        for (name, value) in &self.add {
            headers.append(name.clone(), value.clone());
        }
    }
}

/// The rules of a level, from its snapshot entry: none when it has no
/// table or the table does nothing.
pub fn rules_of(
    message: Option<&pb::HeaderRules>,
) -> Result<Option<std::sync::Arc<Rules>>, String> {
    let Some(message) = message else {
        return Ok(None);
    };

    let rules = Rules::from_message(message)?;
    Ok((!rules.is_empty()).then(|| std::sync::Arc::new(rules)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, value: &str) -> pb::HeaderField {
        pb::HeaderField {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn names_are_removed_then_headers_set_then_added() {
        let rules = Rules::from_message(&pb::HeaderRules {
            remove: vec!["X-Debug".to_owned(), "Server".to_owned()],
            set: vec![field("X-Frame-Options", "DENY"), field("Server", "shop")],
            add: vec![
                field("Link", "</a>; rel=preload"),
                field("link", "</b>; rel=preload"),
            ],
        })
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-debug", HeaderValue::from_static("1"));
        headers.insert("server", HeaderValue::from_static("muru"));
        headers.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
        headers.insert("link", HeaderValue::from_static("</c>; rel=preload"));
        headers.insert("content-type", HeaderValue::from_static("text/html"));
        rules.apply(&mut headers);

        assert!(!headers.contains_key("x-debug"));
        assert_eq!(headers["server"], "shop", "removed, then set");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(headers["content-type"], "text/html");
        let links: Vec<_> = headers.get_all("link").iter().collect();
        assert_eq!(
            links,
            [
                "</c>; rel=preload",
                "</a>; rel=preload",
                "</b>; rel=preload"
            ]
        );
    }

    #[test]
    fn a_table_that_does_nothing_is_no_rules_and_a_bad_name_is_reported() {
        assert_eq!(rules_of(None), Ok(None));
        assert_eq!(rules_of(Some(&pb::HeaderRules::default())), Ok(None));
        let error = rules_of(Some(&pb::HeaderRules {
            remove: vec!["not a name".to_owned()],
            ..Default::default()
        }))
        .unwrap_err();
        assert!(error.contains("is not a header name"), "{error}");
        let error = rules_of(Some(&pb::HeaderRules {
            set: vec![field("X-A", "line\nbreak")],
            ..Default::default()
        }))
        .unwrap_err();
        assert!(error.contains("is not a header value"), "{error}");
    }
}
