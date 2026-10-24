# ASGI scope

```{admonition} Status
:class: note

These are the keys development mode sets today, for `http` scopes. The
`websocket` scope, the `tls` extension and the keys that arrive with
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
| `type` | `http` |
| `asgi` | `{"version": "3.0", "spec_version": "2.4"}` |
| `http_version` | `1.0`, `1.1` or `2`, the client's protocol |
| `method` | The request method |
| `scheme` | `http` or `https` |
| `path` | The request path, percent-decoded, including `root_path` |
| `raw_path` | The path as received, bytes, still encoded |
| `query_string` | The query string, bytes, still encoded |
| `root_path` | Empty until mounts arrive |
| `headers` | A list of `(name, value)` byte pairs, names lower case, in the order received. Hop-by-hop headers never appear. A body sent without `Content-Length` (chunked) is buffered by the edge, which adds a `content-length` pair for it |
| `client` | `(address, port)` of the client |
| `server` | `(address, port)` of the listener |
| `state` | A copy, per request, of the dictionary the application filled during lifespan startup |

## Muru's keys

Everything Muru adds is under `scope["extensions"]["muru"]`, with the same
names as the `muru.*` keys of the [WSGI environ](wsgi-environ.md):

| Key | Value |
| --- | --- |
| `request_id` | The request identifier, also in `X-Request-ID` and the access log |
| `max_body_size` | The body size limit in force, in bytes |
| `tls` | Over HTTPS only: a dictionary with `version`, `cipher`, `server_name` and `alpn` |

No other extension is advertised yet. Sending `http.response.trailers`,
`http.response.pathsend` or `http.response.zerocopysend` raises
`RuntimeError`, as it would under any server that does not announce them.

## Messages

`receive()` returns `http.request` messages with `body` (bytes) and
`more_body`, then `http.disconnect` once the client has gone or the
response is complete. `send()` takes `http.response.start` with `status`
and `headers` (an iterable of `(name, value)` byte pairs, with the same
rules as WSGI headers: no spaces or control characters in names, no CR or
LF in values, a numeric `Content-Length`), and `http.response.body` with
`body` and `more_body`.
