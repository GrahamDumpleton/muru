# ASGI scope

```{admonition} Status
:class: note

These are the keys development mode sets today, for `http` and
`websocket` scopes. The `tls` extension and the keys that arrive with
mounts, virtual hosts and the daemon are listed in
[appendix D](../project/design/appendices/d-environ-keys-scope-extensions-and-environment-variables.md)
of the design specification.
```

Muru builds the scope in Rust from the request the edge received. The
standard keys follow ASGI 3.0 (`spec_version` 2.4); this page lists how
Muru fills them and what it adds.

## Standard keys

| Key | Value |
| --- | --- |
| `type` | `http`, or `websocket` for an upgrade |
| `asgi` | `{"version": "3.0", "spec_version": "2.4"}` |
| `http_version` | `1.0`, `1.1` or `2`, the client's protocol |
| `method` | The request method (`http` scopes only) |
| `scheme` | `http` or `https`; `ws` or `wss` on a `websocket` scope |
| `path` | The request path, percent-decoded, including `root_path` |
| `raw_path` | The path as received, bytes, still encoded |
| `query_string` | The query string, bytes, still encoded |
| `root_path` | Empty until mounts arrive |
| `headers` | A list of `(name, value)` byte pairs, names lower case, in the order received. Hop-by-hop headers never appear. A body sent without `Content-Length` (chunked) is buffered by the edge, which adds a `content-length` pair for it |
| `client` | `(address, port)` of the client |
| `server` | `(address, port)` of the listener |
| `state` | A copy, per request, of the dictionary the application filled during lifespan startup |
| `subprotocols` | On a `websocket` scope: the subprotocols the client offered, in order |

## Muru's keys

Everything Muru adds is under `scope["extensions"]["muru"]`, with the same
names as the `muru.*` keys of the [WSGI environ](wsgi-environ.md):

| Key | Value |
| --- | --- |
| `request_id` | The request identifier, also in `X-Request-ID` and the access log |
| `max_body_size` | The body size limit in force, in bytes |
| `uploads` | Only on an [upload route](../howto/uploads.md) whose request brought files: their descriptions, which `muru.uploads.files(scope)` turns into upload objects |
| `deadline` | When the edge gives up on the request and answers 504 `deadline`, in seconds since the epoch, as `time.time()` counts them; see [timeouts](../howto/timeouts.md) |
| `client_class` | The request's [client class](../howto/client-classes.md), `default` when no rule matched |
| `mode` | Only when the route has a [site mode](../howto/client-classes.md#method-restrictions-and-site-modes), such as `read-only` |
| `precheck` | `True` on an upload route's precheck, the request's headers sent before its body |
| `tls` | Over HTTPS only: a dictionary with `version`, `cipher`, `server_name` and `alpn` |
| `proxy_addr` | Only when a trusted proxy forwarded the request: the address of the proxy that connected, while `client` is the client it forwarded |
| `internal`, `redirect_url`, `redirect_query_string`, `redirect_status` | Only on a request that arrived by [internal redirect](../howto/internal-redirects.md): `True`, the path and query the client asked for, and the status the redirecting application answered; `muru.is_internal(scope)` reads them |
| `error_reason` | Only when the edge asked the application for an [error page](../howto/error-pages.md): the error's reason, while `redirect_status` is its status |
| `auth_request`, `original_method`, `original_uri`, `original_host` | Only on an [authentication sub-request](../howto/auth-subrequests.md): `True`, and the method, path and query, and host of the request being decided about; `internal` is `True` too, and the `redirect_*` keys are absent |

A `websocket` scope also advertises `websocket.http.response`, so the
application may decline the handshake with a response of its own. On
`http` scopes `http.response.pathsend` and `http.response.zerocopysend`
are advertised: the file named, or the open file with its `offset`,
`count` and `more_body`, is sent by the edge from a duplicate of the
descriptor (the [file hints](../howto/file-hints.md) page). Sending
`http.response.trailers` raises `RuntimeError`, as it would under any
server that does not announce it.

## Messages

`receive()` returns `http.request` messages with `body` (bytes) and
`more_body`, then `http.disconnect` once the client has gone or the
response is complete. `send()` takes `http.response.start` with `status`
and `headers` (an iterable of `(name, value)` byte pairs, with the same
rules as WSGI headers: no spaces or control characters in names, no CR or
LF in values, a numeric `Content-Length`), and `http.response.body` with
`body` and `more_body`.

On a `websocket` scope, `receive()` returns `websocket.connect`, then
`websocket.receive` messages with `text` or `bytes`, then
`websocket.disconnect` with the close `code` and `reason`. `send()` takes
`websocket.accept` (`subprotocol`, `headers`), `websocket.send` (`text` or
`bytes`), `websocket.close` (`code`, `reason`) and, before accepting,
`websocket.http.response.start` and `websocket.http.response.body`.
