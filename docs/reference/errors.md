# Error responses

```{admonition} Status
:class: note

These are the responses the edge generates today. Reasons for releases,
rollouts and upstreams arrive with those features;
[appendix C](../project/design/appendices/c-configuration-reference.md)
of the design specification has the full planned list.
```

When Muru itself answers a request, rather than your application, the
response has a status and a reason. The reason names the cause more
precisely than the status, appears in the access log line as
`reason=...`, and is stable, so scripts and alerts can match on it.

## Statuses and reasons

| Status | Reason | When |
| --- | --- | --- |
| 400 | `malformed` | The request body could not be read: a broken chunked encoding, or the connection failed mid-body |
| 404 | `internal_only` | The route or mount is reachable only through an internal redirect (see the [internal redirects guide](../howto/internal-redirects.md)) |
| 408 | `client_timeout` | The client paused for longer than the body idle timeout while sending a body |
| 413 | `body_too_large` | The declared or actual body is over the limit in force; the page names the limit and the setting. Declared lengths are refused before the body is read |
| 500 | `app_error` | The application raised, or called `start_response` with `exc_info` after output had started. Development mode includes the traceback |
| 500 | `redirect_loop` | The application redirected internally more than 5 times for one request |
| 502 | `worker_crashed` | The worker's connection ended before a response head, or it sent something invalid |
| 503 | `maintenance` | The application is in maintenance mode (`muructl maintenance on`) and nothing exempted the request; `Retry-After` is the release's `retry_after`, 60 s by default (see the [error pages guide](../howto/error-pages.md)) |
| 503 | `no_workers` | No worker is available at all, with `Retry-After: 1` |
| 503 | `queue_full` | Every worker thread was busy for longer than the queue timeout, with `Retry-After: 1` |
| 504 | `deadline` | The application did not produce a response head within the request deadline |
| 504 | `soft_timeout` | The request ran past the soft timeout and was interrupted in the worker before it produced a response head; development mode includes the traceback of where it was (see the [timeouts guide](../howto/timeouts.md)) |
| any | `error_override` | The application's own response had a status that `error_override` lists, and the edge replaced it with the error page; the status is the application's |

The HTTP parser answers a few things itself, before a request exists: a
request head over the size limit, too many headers, an invalid request
line or conflicting length headers get a `400` or `431` from the parser
and the connection is closed. Those carry no reason and no request id.

## The page

A custom page configured for the status, or the status and reason,
replaces the body described here; the [error pages
guide](../howto/error-pages.md) covers those. Otherwise the body is
negotiated from the `Accept` header:

- `text/html` gets a small HTML page with the status, the description, the
  request id and, in development mode, the traceback;
- `application/json` or `application/problem+json` gets a problem
  document (RFC 9457) with `Content-Type: application/problem+json`;
- anything else gets plain text with the same fields.

Every error response has `Cache-Control: no-store`, a `Content-Length`,
the `X-Request-ID` header, and `Retry-After` when the table above says so.
A `413` on HTTP/1.1 also closes the connection, since the client may still
be sending.

The problem document looks like this; `traceback` is present only in
development mode:

```json
{
  "type": "about:blank",
  "title": "Internal Server Error",
  "status": 500,
  "detail": "The application raised RuntimeError: deliberate failure.",
  "reason": "app_error",
  "request_id": "01M5P6WZC1740HFT2N80MAV4YV",
  "traceback": "Traceback (most recent call last):\n  ..."
}
```

## Finding the request

The request id in the page is the one in the access log line and in the
response header, so a user's screenshot of an error page leads straight to
the log line and the worker that served it. Ids are ULIDs: sortable by
time, unique across workers, and safe to put in a URL.
