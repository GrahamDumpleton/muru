---
status: accepted
date: 2026-11-06
---

# ADR 0049: A virtual host's settings are a site's defaults, and what is read before the host is known belongs to the listener

## Context and problem statement

Section 4.3 lists the settings a virtual host carries for its routes:
trusted proxies, limits, error pages, added headers, the access log's
destination and format, maintenance mode, and the canonical and HTTPS
redirects. Section 4.7 says the proxy settings can be overridden per
listener and per host, and appendix C places the limits on the request
head (`max_header_bytes`, `max_headers`, `header_read_timeout`) on the
listener and the host. Section 3.3 orders the layers with a virtual
host's settings after a release's, so a host would override what an
application ships, while section 4.12 consults error pages in the order
route, release, host, node.

Implementing the host level (#171) raised two questions. Which of the
listed settings can a host carry at all, given what the edge knows when
it applies each? And where does the host stand between the node and a
release, now that limits and the access log can be set at each level?

## Decision drivers

- The edge resolves the forwarding headers before it selects a host,
  because `X-Forwarded-Host` from a trusted proxy is what names the
  host. A host's own proxy settings could only be read after the node's
  or the listener's had already decided which host the request is for,
  and the client address and scheme would then be resolved under
  different trust than the host name was.

- The limits on the request head are enforced while the head is read.
  The host is known only once it has been read. A TLS connection's SNI
  name could choose them, a plain HTTP connection has nothing to choose
  by, and an HTTP/2 connection carries requests for several hosts.

- A setting should resolve the same way at every level, so that one
  rule explains error pages, limits and the access log.

- A route is written by whoever operates the node and names one path of
  one host. A release's settings ship with the application's code. A
  host's settings cover every application the site routes to.

## Considered options

- Keep the specification as written: proxy settings and head limits per
  host, chosen by SNI where there is one and by a second pass after host
  selection otherwise, with a host overriding a release.

- A host carries only what the edge applies after it has selected the
  host. A host's value is a default for the site that a release refines
  and a route decides.

## Decision outcome

Chosen option: the second.

- A virtual host does not carry proxy settings or limits on the request
  head. They belong to the listener (#316, #155) and the node.

- A host carries the limits on one request (`max_body_size` and a
  `timeouts` table of `queue_timeout`, `request_deadline`,
  `body_idle_timeout` and `response_idle_timeout`), the access log's
  choice of records, error pages and the override, rate limits, added
  headers (#325), compression (#152), maintenance mode (#402), and the
  canonical and HTTPS redirects.

- For a request, a setting is the first that sets it of: the route, the
  mount or worker group that serves it, the release, the virtual host,
  the node, the default. That is the order error pages already had. For
  the access log the application stands where the release does. Lists
  that only exclude, such as the access log's excluded paths, add up
  across the levels, and rate limits are drawn from at every level that
  names them.

- The limits on a request are those of the route the client's own
  request matched. An internal redirect or an error page handler keeps
  them, since the deadline bounds the whole chain.

### Consequences

- Good: one rule for every inherited setting, and it is the one error
  pages follow.

- Good: every setting a host carries is applied with the host known, so
  there is no second resolution of who the client is.

- Good: an application can ship the limits its endpoints need, and the
  operator's route still has the last word.

- Bad: an application's own table can raise a body limit or a deadline
  above the one its host sets. An operator who must cap them sets the
  limit on the routes.

- Bad: a node that needs different proxy trust for two sites has to
  serve them on different listeners.

## More information

Sections 3.3, 4.3, 4.6, 4.7, 4.12 and 11.2, and appendix C. Issues
#171, #155, #316 and #404.
