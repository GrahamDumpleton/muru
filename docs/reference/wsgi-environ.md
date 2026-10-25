# WSGI environ

```{admonition} Status
:class: note

These are the keys development mode sets today. Keys that arrive with
mounts, virtual hosts, trusted proxies and the daemon are listed in
[appendix D](../project/design/appendices/d-environ-keys-scope-extensions-and-environment-variables.md)
of the design specification.
```

Muru builds the environ in Rust from the request the edge received. The
standard keys follow PEP 3333; this page lists how Muru fills them and what
it adds.

## Standard keys

| Key | Value |
| --- | --- |
| `REQUEST_METHOD`, `PATH_INFO`, `QUERY_STRING` | From the request line. `SCRIPT_NAME` is empty until mounts arrive |
| `SERVER_PROTOCOL` | `HTTP/1.0`, `HTTP/1.1` or `HTTP/2.0`, the client's protocol |
| `SERVER_NAME`, `SERVER_PORT` | The listener's address and port |
| `REMOTE_ADDR`, `REMOTE_PORT` | The client's address and port |
| `CONTENT_TYPE`, `CONTENT_LENGTH` | From the headers. A body sent without `Content-Length` (chunked) is buffered by the edge, so `CONTENT_LENGTH` is set for it too |
| `HTTP_*` | Every other header, upper-cased with dashes as underscores. Hop-by-hop headers (`Connection`, `Transfer-Encoding`, `Upgrade` and the others) never appear, and neither does a header whose name contains an underscore, since it could masquerade as another header's key |
| `HTTPS` | `on` over HTTPS, absent otherwise |
| `REDIRECT_URL`, `REDIRECT_QUERY_STRING`, `REDIRECT_STATUS` | Present on a request that arrived by [internal redirect](../howto/internal-redirects.md): the path and query the client asked for, and the status the redirecting application answered |
| `wsgi.version` | `(1, 0)` |
| `wsgi.url_scheme` | `http` or `https` |
| `wsgi.input` | A Rust-backed stream: `read`, `readline`, `readlines` and iteration return an empty value at the end of the body and never block past it |
| `wsgi.input_terminated` | Always `True`: the body is framed, so reading to the end is safe without `CONTENT_LENGTH` |
| `wsgi.errors` | Writes to the worker's standard error, which the terminal shows tagged with the worker |
| `wsgi.multithread` | `True` when the worker has more than one application thread |
| `wsgi.multiprocess` | `True`, except for a single-process `muructl run`, where it is `False` |
| `wsgi.run_once` | `False` |
| `wsgi.file_wrapper` | `muru._native.FileWrapper`: iterates a file in blocks, with `close()` closing the file. Passing the descriptor to the edge is phase 2 |

## Keys Muru adds

| Key | Value |
| --- | --- |
| `muru.request_id` | The request id, a 26-character ULID. The same value is in the `X-Request-ID` response header, the access log line and the error page |
| `muru.max_body_size` | The body limit in force, in bytes: 100 MB in development, 10 MB with `--allow-public`, or what `--max-body-size` set |
| `muru.proxy_addr` | Present only when a trusted proxy forwarded the request (see [behind a proxy](../howto/behind-a-proxy.md)): the address of the proxy that connected, while `REMOTE_ADDR` is the client it forwarded |
| `muru.internal` | `True` on a request that arrived by internal redirect, absent otherwise; `muru.is_internal(environ)` reads it |
| `muru.error_reason` | Present only when the edge asked the application for an [error page](../howto/error-pages.md): the error's reason, for example `app_error`, while `REDIRECT_STATUS` is its status |
| `muru.auth_request`, `muru.original_method`, `muru.original_uri`, `muru.original_host` | Present only on an [authentication sub-request](../howto/auth-subrequests.md): `True`, and the method, path and query, and host of the request being decided about. `muru.internal` is `True` too, and the `REDIRECT_*` keys are absent |
| `muru.tls` | Present over HTTPS only: a dictionary with `version` (`TLSv1.3` or `TLSv1.2`), `cipher` (the negotiated suite, for example `TLS13_AES_256_GCM_SHA384`), `server_name` (the name the client asked for, empty when it connected by address) and `alpn` (`h2`, `http/1.1`, or empty when the client offered none) |

## Rules worth knowing

- **One request, one context.** Each request runs in a copy of its
  thread's `contextvars` context, so a context variable set during one
  request is not visible to the next request on the same thread.
  Thread-locals do persist, because the application threads live for the
  whole worker: a `threading.local` database connection set up by one
  request is there for later requests on that thread.
- **start_response is checked.** A status that is not three digits, a
  space and a reason, headers that are not a list of `(str, str)` tuples,
  names or values with control characters, or a second call without
  `exc_info` all raise (`ValueError` for a bad value, `TypeError` for a
  wrong type). The reason phrase reaches HTTP/1.1 clients.
- **Content-Length is enforced.** Output beyond a declared length is
  dropped; a body that ends short of it fails the response and closes the
  connection, with a line in the log.
- **100 Continue is the edge's job.** A client that sends
  `Expect: 100-continue` gets the interim response when your code first
  reads `wsgi.input`, or at once when the body was buffered. Over HTTP/2 the
  interim response is not sent yet.
- **Exceptions are a 500, never a dead worker.** This includes
  `SystemExit` and `KeyboardInterrupt` raised by application code.
