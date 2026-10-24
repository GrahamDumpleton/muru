# Logging

```{admonition} Status
:class: note

Development mode: the terminal, or a file. The log collector, rotation and
Python logging defaults arrive with the daemon.
```

A development run writes two kinds of lines to the terminal: what your
application and Muru's own processes print, and one access log line per
request.

## Application output

Everything a worker writes to standard output or standard error is relayed
by the host with the worker's index, so output from different workers can
be told apart:

```
muru.worker[0]: Traceback (most recent call last):
muru.worker[1].stdout: starting import of catalogue
```

Lines from the host itself carry `muru.host[PID]:`, and lines from muructl
start with `muru:`. Python's `logging` goes to standard error by default
and so arrives tagged; nothing needs configuring.

## The access log

```
05:42:22.836 200 GET /hello 18B 0.7ms queue=0.2ms app=0.3ms worker=0 id=01M5P6WZBK740HFT2N80MAV4YV
05:42:22.849 500 GET /error 334B 7.8ms queue=0.0ms app=0.0ms reason=app_error worker=0 id=01M5P6WZC1740HFT2N80MAV4YV
```

Time, status, method, target, response size, total time, time spent
waiting for a worker thread, time in the application, the reason when Muru
generated the response, the worker, and the request id that the
`X-Request-ID` response header and the error page carry too.

`--access-log` chooses where the lines go:

| Value | Effect |
| --- | --- |
| `terminal` (default) | compact lines as above |
| `errors` | only 4xx and 5xx responses and requests slower than `--slow` (default `1s`) |
| `off` | nothing |
| a file path | JSON lines appended to the file, one object per request |

`--access-log-exclude PATTERN`, repeatable, hides requests whose path
matches: `*` matches any run of characters and `?` one, so
`--access-log-exclude '/static/*' --access-log-exclude '/healthz'` keeps
assets and probes out of the terminal.

Whatever is shown, the last thousand records are kept in memory and the
per-second counts for the last minute, which is what the dashboard
displays.

A JSON line has these fields:

```json
{"time": "2026-10-24T05:42:22.836Z", "request_id": "01M5P6WZBK740HFT2N80MAV4YV",
 "client": "127.0.0.1", "method": "GET", "target": "/hello", "version": "HTTP/1.1",
 "status": 200, "reason": "", "bytes": 18, "worker": 0,
 "queue_ms": 0.2, "app_ms": 0.3, "total_ms": 0.7}
```

`reason` is empty for responses the application produced and names the
cause for ones Muru generated; the [error responses](../reference/errors.md)
page lists them.

## Log files and rotation

Only the access log can go to a file today. On SIGUSR1 each worker runs the
`on_reopen_logs` hook, which is where an application that writes its own
log files reopens them; rotation of Muru's own files is part of the daemon.
