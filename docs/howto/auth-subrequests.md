# Authentication sub-requests

```{admonition} Status
:class: note

A route's `auth` table: the handler sub-request, the allow, deny,
redirect and fail-closed outcomes, copied headers, the decision cache,
and guarded static directories and WebSocket upgrades. HTTP upstreams
as handlers or targets arrive with upstreams.
```

A route can require a decision from an application before the edge
serves it. The edge sends a short sub-request to an application path and
goes on only when that answers 2xx. This guards what cannot decide for
itself, such as a static directory or a WebSocket upgrade, without the
guarded request's body passing through Python, and lets one application
be the gatekeeper for others.

```toml
[[route]]
host = 'shop'
path = '/media/'
static = '/srv/shop/media'
auth = { app = 'shop', path = '/_auth/media', copy_headers = ['X-Auth-User'] }

[[route]]
host = 'shop'
path = '/ws/'
app = 'chat'
auth = { app = 'shop', path = '/_auth/ws' }
```

| Key | Default | Meaning |
| --- | --- | --- |
| `app` | required | the application asked |
| `path` | required | the path asked, usually of an [internal mount or route](internal-redirects.md) |
| `timeout` | `2s` | how long the handler has, queueing included |
| `copy_headers` | none | headers of an allowing answer added to the guarded request for its target |
| `cache` | off | `{ ttl = '30s', key = ['cookie:sessionid', 'header:authorization'] }` remembers allowing decisions |

## The sub-request

The handler gets a `GET` (a `HEAD` stays `HEAD`) for `path` with the
client's headers and no body. It is routed like an [internal
redirect](internal-redirects.md): an internal mount or route of the
application is open to it, and the application's serving release
answers, or the one a preview or sticky cookie picks. The handler sees:

| WSGI environ | ASGI `extensions["muru"]` | Value |
| --- | --- | --- |
| `muru.auth_request` | `auth_request` | `True` |
| `muru.original_method` | `original_method` | the guarded request's method |
| `muru.original_uri` | `original_uri` | its path and query |
| `muru.original_host` | `original_host` | the host it asked for |
| `muru.internal` | `internal` | `True`, as for any sub-request of the edge's |

The same three values travel as the `X-Original-Method`,
`X-Original-URI` and `X-Original-Host` headers, which a client can never
supply. `REDIRECT_URL` and its companions are not set.

```python
def auth(environ, start_response):
    user = session_user(environ.get("HTTP_COOKIE", ""))

    if user is None:
        start_response("401 Unauthorized", [("WWW-Authenticate", 'Bearer realm="shop"')])
        return [b""]

    if not user.may_read(environ["muru.original_uri"]):
        start_response("403 Forbidden", [])
        return [b""]

    start_response("200 OK", [("X-Auth-User", user.name)])
    return [b""]
```

## Outcomes

| Handler answers | The client gets |
| --- | --- |
| 2xx | the guarded request served, with the `copy_headers` of the answer added to it. A client's own copies of those headers are always removed, so the target can trust them |
| 401 or 403 | the handler's response as it was: its status, body, `WWW-Authenticate`, `Set-Cookie` and the rest |
| 3xx | the handler's response, so a redirect to a login page works |
| anything else, an error, an internal redirect, or no answer within `timeout` | `503` with the reason `auth_unavailable`: the check fails closed |

A refusal is logged with the reason `auth_denied` and the handler's
status. A [custom error page](error-pages.md) for `401:auth_denied` or
`403:auth_denied` replaces the handler's body, keeping its
`WWW-Authenticate`, `Set-Cookie`, `Location` and `Retry-After`; a page
for the plain status does not, since the handler's body is the
application's own.

The guarded request's body is not read until the check passes, and a
client waiting on `Expect: 100-continue` hears nothing before then. For
a WebSocket, the check runs before the upgrade is accepted, so a refused
client sees the handler's 401 or 403 as the answer to its upgrade.

In the access log the guarded request is one line, with the handler's
path in `redirects=`; the sub-request counts in the metrics of the
application that answered it, and `muru_auth_requests_total` counts
decisions by result: `allowed`, `cached`, `denied` or `unavailable`.

## Caching decisions

```toml
[[route]]
host = 'shop'
path = '/media/'
static = '/srv/shop/media'

[route.auth]
app = 'shop'
path = '/_auth/media'
copy_headers = ['X-Auth-User']
cache = { ttl = '30s', key = ['cookie:sessionid'] }
```

With a `cache`, an allowing decision and its copied headers are
remembered for `ttl`, keyed by the listed cookies and headers of the
request, so a busy static path does not cost one Python call per file.
Only allowing decisions are cached, a request that carries none of the
listed cookies or headers is never served from the cache, and the cache
is per edge process and bounded, so an entry may be dropped early but
never kept past its `ttl`. Logging out cannot revoke a cached decision
before then: keep `ttl` short.

## Capacity

The handler runs in the application's own workers, so a flood of guarded
requests queues behind the application's work and the check can time
out. Give the handler's path its own [worker group](worker-groups.md),
or make a small application the gatekeeper, so the check is never
routed through the capacity it protects.
