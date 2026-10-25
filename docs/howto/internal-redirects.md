# Internal redirects

```{admonition} Status
:class: note

Internal redirects from an application to a static route or another
application path, internal routes and mounts, and the `X-Muru-Redirect`,
`X-Accel-Redirect` and CGI local redirect forms. Upload offloading, the
reverse direction, is still to come.
```

An application can hand a request back to the edge to be served from
somewhere else: a file in a directory clients cannot reach directly, or
another path of the same or another application. The application decides,
for example whether the user may download the file, and the edge does the
work, so a worker thread is not held while a large file goes out.

## A protected download

Give the files an internal static route. Clients asking for it directly
get a 404, and only redirects from the applications it names reach it:

```toml
[[route]]
host = 'site'
path = '/'
app = 'shop'

[[route]]
host = 'site'
path = '/protected/'
static = '/srv/shop/private'
internal = true
accept_redirects_from = ['shop']
```

The application answers with `X-Muru-Redirect` instead of the file:

```python
def download(environ, start_response):
    name = authorised_file(environ)   # or a 403

    start_response("200 OK", [
        ("X-Muru-Redirect", f"/protected/{name}"),
        ("Content-Disposition", f'attachment; filename="{name}"'),
        ("Cache-Control", "private"),
    ])
    return [b""]
```

The edge drops that response, routes `/protected/NAME` as a new request
and serves the file as any static file: with its `Content-Type` from the
name, `ETag`, `Last-Modified`, ranges and conditional requests, since the
client's own headers travel with the sub-request. `Content-Disposition`,
`Cache-Control`, `Expires`, `Set-Cookie` and `Vary` from the
application's response are kept on the file's response, and so is
`Content-Type` when the application sends a non-empty one. Frameworks
that always set a `Content-Type` need it emptied for the file's type to
apply; in Django, `response["Content-Type"] = ""`.

## Redirecting to an application path

The target can be a path served by an application. A redirect back into
the same application goes to the release that redirected, so a
deployment in progress cannot split one request across two releases;
another application's route takes the redirect only when it lists the
redirecting application in `accept_redirects_from`, and its own traffic
policy chooses the release.

The sub-request is a `GET` (a `HEAD` stays `HEAD`) with the client's
headers, unless the response also carries `X-Muru-Redirect-Method:
preserve`, which keeps the method and the body where the edge buffered
the body (one sent without a `Content-Length`); a body with a declared
length streams to the application as it arrives and cannot be sent
again. `X-Muru-Redirect-Host: NAME` routes the sub-request under another
configured host, by its name in the configuration or one of its names.
`X-Muru-Redirect-Header-NAME: value` adds a trusted header `NAME` to the
sub-request, replacing any the client sent with that name.

The target sees how it was reached:

| Where | What |
| --- | --- |
| WSGI environ | `REDIRECT_URL` and `REDIRECT_QUERY_STRING`, the path and query the client asked for, `REDIRECT_STATUS`, the status the redirecting application answered, and `muru.internal` |
| ASGI scope | `scope["extensions"]["muru"]`: `internal`, `redirect_url`, `redirect_query_string` and `redirect_status` |
| Python | `muru.is_internal(environ_or_scope)` |

A client's own `X-Muru-*` headers never reach an application, so a
header only an internal request carries can be trusted on one.

## Internal routes and mounts

`internal = true` on a route, or on a mount in the application's
`[mounts.NAME]` table, keeps clients out: a request for the path is
answered 404 `internal_only`. A redirect from the route's own application
(for a static route, its `fallback` application) always reaches it;
another application's only when `accept_redirects_from` names it, which
a static route with no fallback needs for any redirect at all. An
internal mount accepts the release's own redirects, and other
applications' when its `accept_redirects_from` names them.

## The other redirect forms

Two conventions from other servers are honoured when a release or a mount
opts in:

```toml
[tool.muru]
accel_redirect = true        # nginx's X-Accel-Redirect: /path
cgi_local_redirect = true    # a 200 with Location: /path and no body

[tool.muru.mounts.reports]
path = "/reports"
entrypoint = "shop.reports:application"
accel_redirect = false       # this mount's own choice
```

Without the setting the headers pass through to the client like any
other. `X-Muru-Redirect` is always honoured and never reaches a client.

## Limits

A chain of redirects stops after 5 hops with 500 `redirect_loop`. The
request deadline covers the whole chain. A redirect to a path that is
not absolute, or that names another server, is ignored and the response
sent as it is, with a line in the edge's log. A WebSocket upgrade cannot
be redirected once a WSGI application has answered it. The access log
writes one line for the chain, with the client's target and
`redirects=` listing where it went; a redirect hop counts toward the
redirecting application's request metrics, and the sub-request's id is
the client's with `-1`, `-2` and so on appended.
