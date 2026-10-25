# Custom error pages and maintenance mode

```{admonition} Status
:class: note

Custom pages by status or status and reason at the node, host, release
and route levels, as a file, a template or an application path; the
error override; and `muructl maintenance on` with exemptions by path,
address and cookie. The dashboard's maintenance control is still to
come.
```

When Muru answers a request itself, the response is one of the
[built-in error pages](../reference/errors.md): negotiated between HTML,
a problem document and plain text, with the status, the reason and the
request id. Custom pages replace them, and can replace an application's
own error responses as well.

## Pages

An `error_pages` table maps a status, or a status and reason joined by a
colon, to a page:

```toml
[app.shop.release.r1.error_pages]
"503:maintenance" = { file = 'pages/maintenance.html' }
503 = { file = 'pages/busy.html' }
404 = { template = '<h1>{status} {title}</h1><p>Nothing at {path}.</p>' }
500 = { path = '/errors/500' }
```

A page is one of:

| Key | Meaning |
| --- | --- |
| `file` | a file sent as the body, with the content type its name implies; a release's file may be relative to its source directory, elsewhere the path is absolute. The file is read when the configuration is applied, so a change needs a new release or another apply |
| `template` | text the edge renders, with the placeholders below filled in; `content_type` sets its type (default `text/html; charset=utf-8`), and values are HTML-escaped when the type is HTML |
| `path` | a path of the application, which the edge asks for the page as an internal sub-request |

Templates know `{status}`, `{title}` (for example `Service Unavailable`),
`{reason}`, `{description}`, `{request_id}`, `{retry_after}` (empty when
the response has none), `{host}` and `{path}`. Anything else in braces is
left as written.

Every page is sent with the error's status, `Cache-Control: no-store`,
the `X-Request-ID` header and the `Retry-After` the built-in page would
carry. Tracebacks in development mode are a feature of the built-in page
only.

### Where pages are configured

| Level | Table |
| --- | --- |
| route | `error_pages` on a `[[route]]`, as an inline table or `[route.error_pages]` |
| release | `[app.NAME.release.ID.error_pages]`, or `[tool.muru.error_pages]` in the project, which `muructl run` uses as well |
| host | `[host.NAME.error_pages]` |
| node | `[error_pages]` in the daemon's own configuration file, re-read by `muructl daemon reload` |

For an error, the edge looks at the levels in that order and the first
one that has a page for the status decides; within a level a
`status:reason` entry beats the plain status. So a release's `404` page
does not answer a host's `404:no_route`, since the host level is only
consulted when the route and release say nothing about 404 at all. A
status is any from 400 to 599; the reasons are the ones listed in the
[error reference](../reference/errors.md).

Errors the edge answers before it has chosen an application, such as
`no_route` or `misdirected`, see only the route (when one matched), host
and node levels.

## An application serves the page

With `path`, the edge sends a `GET` (a `HEAD` stays `HEAD`) for that
path through routing as an internal sub-request, with the client's
headers and no body, and uses what comes back with the error's own
status and `Retry-After`. The handler sees the request as an
[internal redirect](internal-redirects.md) does, with `REDIRECT_URL`,
`REDIRECT_QUERY_STRING` and `REDIRECT_STATUS` (the error's status), and
`muru.error_reason` in the WSGI environ, or `error_reason` under
`extensions["muru"]` in an ASGI scope, holding the reason:

```python
def error_page(environ, start_response):
    status = environ["REDIRECT_STATUS"]
    reason = environ["muru.error_reason"]
    body = render("error.html", status=status, reason=reason).encode()
    start_response("200 OK", [("Content-Type", "text/html; charset=utf-8")])
    return [body]
```

The handler answers with any status below 400; the edge replaces it with
the error's. Internal routes and mounts are open to the sub-request, and
a path in another application needs no `accept_redirects_from`.

Safeguards keep a broken handler from making things worse:

- the handler has 2 s, queueing included, from when it is asked;
- an application is not asked for its own page when the error was one
  of capacity (`no_workers`, `queue_full`, `cold_start_timeout`,
  `draining`, `release_failed` or `edge_overloaded`), since its workers
  are what failed;
- a handler that fails, answers 400 or above, or redirects internally,
  leaves the built-in page in place, so an error never recurses;
- the response gets `Cache-Control: no-store` unless the handler set
  its own.

In the access log the failed request is one line with the error's
status and reason, with the handler's path in `redirects=`; the handler
counts in the metrics of the application that served it.

## Replacing the application's own error responses

An application's `404` or `500` is its own response, sent as the
application wrote it. `error_override` replaces listed statuses with the
error page instead:

```toml
[app.shop.release.r1]
error_override = { statuses = [404, 500, 502, 503], when = 'html_only' }
```

| Key | Meaning |
| --- | --- |
| `statuses` | the statuses replaced, from 400 to 599 |
| `when` | `all` (the default), `empty_body` (only when the response head says there is no body), `not_json` (only when the response is not `application/json` or `application/problem+json`), or `html_only` (only when the client accepts HTML, so API clients keep the application's document) |

The table goes at the same four levels as `error_pages`, and the most
specific level that has one decides. A replaced response is logged with
the reason `error_override`, and its page is looked up by that reason,
so `"404:error_override"` names a page for the application's own 404s
alone. `Retry-After`, `WWW-Authenticate` and `Allow` are kept from the
application's response.

To keep one particular response, the application sets
`X-Muru-Error-Override: off` on it. Response headers starting with
`X-Muru-` instruct the edge and never reach the client.

The override applies before any body byte is sent: a response that has
started streaming is never replaced.

## Maintenance mode

```console
$ muructl maintenance on shop
muru: shop is in maintenance: clients get 503 maintenance until `muructl maintenance off shop`
```

While it is on, the edge answers every request routed to the application
with `503` and the reason `maintenance`, carrying `Retry-After`, without
troubling the workers. The page is the one for `503:maintenance`, or
`503`, at any level, else the built-in page. `muructl app list` marks the
application `[maintenance]`, the event `maintenance.changed` records
each switch, and the state survives a daemon restart.

What is let through is configured on the release:

```toml
[app.shop.release.r1.maintenance]
exempt_paths = ['/health', '/static/']
exempt_addresses = ['10.0.0.0/8', '2001:db8::/32']
bypass_cookie = 'muru_bypass=letmein'
retry_after = '5m'
```

| Key | Default | Meaning |
| --- | --- | --- |
| `exempt_paths` | none | path prefixes served as usual |
| `exempt_addresses` | none | client addresses, or CIDR ranges, served as usual; the client is the one a trusted proxy forwarded, where there is one |
| `bypass_cookie` | none | a `name=value` cookie that lets a client through, for checking the site before turning maintenance off |
| `retry_after` | `60s` | the `Retry-After` the page carries |

Static routes are served by the edge and are not affected; a static
route's fallback application is. Internal redirects and error page
sub-requests from an application that was served are let through as
well.

```console
$ muructl maintenance off shop
muru: shop serves as usual again
```

## Checking the configuration

`muructl apply` refuses a table it cannot use: a key that is not a
status, a page with none or more than one of `file`, `template` and
`path`, a relative file outside a release, a release file that does not
exist, an unknown `when`, or a malformed exemption. The daemon refuses
to start on a bad `[error_pages]` table in its own file, as it does for
`[proxy]`.
