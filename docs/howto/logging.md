# Logging

```{tip}
`muructl logging level APP NAME=LEVEL --for 10m` changes a logger's level
in a release's workers for a period, without a restart; the
[inspecting workers guide](debugging.md) has the details.
```

A development run writes two kinds of lines to the terminal: what your
application and Muru's own processes print, and one access log line per
request. The daemon writes the same lines to files, one per release and
one access log, which `muructl logs` reads.

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
and so arrives tagged; nothing needs configuring. On the terminal these
lines have no time in front; [times on log lines](#times-on-log-lines)
says where they do and how to ask for one.

## Python logging

Before the application is imported, the host sets the root logger's level
and replaces `logging.lastResort`, the handler Python falls back on for a
logger with no handler of its own, with one that writes to standard error
in the platform format. Nothing else is touched: `basicConfig` and
`dictConfig` behave exactly as they do without Muru, because the
last-resort handler is used only where no handler exists.

The settings come from a `[tool.muru.logging.python]` table in
pyproject.toml or muru.toml, or from the `logging.python` table of a
declared release, which wins:

```toml
[tool.muru.logging.python]
level = "INFO"
format = "json"
capture_warnings = true
loggers = { urllib3 = "WARNING" }
```

| Key | Default | Effect |
| --- | --- | --- |
| `level` | `WARNING`, Python's own | the root logger's level: `DEBUG`, `INFO`, `WARNING`, `ERROR` or `CRITICAL` |
| `format` | `standard` | `standard` writes `LEVEL logger: message`, with no time since Muru adds one where the output wants it; `json` writes one object per record with the time, level, logger, message, application, release, group and worker, the request id during a request, and the exception when there is one; anything else is a `logging` format string |
| `capture_warnings` | `false` | route `warnings.warn` through the `py.warnings` logger |
| `loggers` | none | levels for named loggers |

A worker's records arrive tagged like the rest of its output, so a
standard record reads `muru.worker[0]: WARNING demo: warned`. The host
sets `MURU_LOG_LEVEL` and `MURU_LOG_FORMAT` to what was chosen, and
`muru.logging.default_config()` returns the same defaults as a
`dictConfig` dictionary, for an application that configures logging
itself and wants to start from them:

```python
import logging.config

import muru

logging.config.dictConfig(muru.logging.default_config())
```

Every record carries the request it was logged in and where it was
logged, as attributes an application's own format can use:

| Attribute | Value |
| --- | --- |
| `muru_request_id` | the request's id, as in the [access log](#the-access-log), or `None` outside a request |
| `muru_app` | the application |
| `muru_release` | the release |
| `muru_group` | the worker group |
| `muru_worker` | the worker's index in its group, or `None` in the host |

```python
handler.setFormatter(
    logging.Formatter("%(levelname)s [%(muru_request_id)s] %(name)s: %(message)s")
)
```

The `muru_` prefix keeps them apart from what an application passes in
`extra`, so a middleware that logs with `extra={"request_id": ...}` keeps
working.

## The access log

```
05:42:22.836 200 GET /hello 18B 0.7ms queue=0.2ms app=0.3ms worker=0 id=01M5P6WZBK740HFT2N80MAV4YV
05:42:22.849 500 GET /error 334B 7.8ms queue=0.0ms app=0.0ms reason=app_error worker=0 id=01M5P6WZC1740HFT2N80MAV4YV
```

The time the request arrived, status, method, target, response size,
total time, time spent waiting for a worker thread, time in the
application, the reason when Muru generated the response, the worker,
and the request id that the `X-Request-ID` response header and the error
page carry too. On a terminal the time is your local clock; when the
output goes to a pipe or a file, or with `--timestamps`, it is the full
UTC time, as in `2026-11-02T05:42:22.836Z 200 GET /hello ...`.

A WebSocket has two lines with the same request id. The first is written
when the application accepts it, with the 101 and the handshake's time;
the second when it closes, with how long it was open, the bytes sent to
the client, the side that closed first, the close code, and the messages
received from the client and sent to it:

```
05:42:23.010 101 GET /ws/chat/42 0B 0.8ms queue=0.0ms app=0.7ms worker=0 id=01M5P6WZD3740HFT2N80MAV4YV
05:47:26.402 101 GET /ws/chat/42 12kB 5m03s websocket=closed by=client code=1000 received=12 sent=40 worker=0 id=01M5P6WZD3740HFT2N80MAV4YV
```

`--access-log` chooses where the lines go:

| Value | Effect |
| --- | --- |
| `terminal` (default) | compact lines as above |
| `errors` | only 4xx and 5xx responses, requests slower than `--slow` (default `1s`), and WebSockets that closed other than normally or going away |
| `off` | nothing |
| a file path | JSON lines appended to the file, one object per request |

`--access-log-exclude PATTERN`, repeatable, hides requests whose path
matches: `*` matches any run of characters and `?` one, so
`--access-log-exclude '/static/*' --access-log-exclude '/healthz'` keeps
assets and probes out of the terminal. Under the daemon the `[logs]`
table makes the same choices, as [what the daemon's access log
writes](#what-the-daemons-access-log-writes) describes.

Whatever is shown, the last thousand records are kept in memory and the
per-second counts for the last minute, which is what the dashboard
displays.

A JSON line has these fields:

```json
{"time": "2026-10-24T05:42:22.836Z", "request_id": "01M5P6WZBK740HFT2N80MAV4YV",
 "client": "127.0.0.1", "app": "shop", "release": "r1", "host": "shop.example.com",
 "group": "default", "mount": "default", "method": "GET", "target": "/hello",
 "version": "HTTP/1.1", "status": 200, "reason": "", "bytes": 18, "worker": 0,
 "retried": false, "queue_ms": 0.2, "app_ms": 0.3, "total_ms": 0.7, "snapshot": 3,
 "redirects": [],
 "client_class": "default", "websocket": null, "handoff": null}
```

`reason` is empty for responses the application produced and names the
cause for ones Muru generated; the [error responses](../reference/errors.md)
page lists them. `app` and `release` name what served the request, `host`
is the virtual host that matched or else the host the client asked for,
`group` and `mount` are `default` or the named [worker
group](worker-groups.md) and [mount](mounts.md), and `snapshot` is the
version of the routing the edge used (0 in development mode).
`redirects` lists the targets of [internal redirects](internal-redirects.md)
the request went through, and `client_class` is its [client
class](client-classes.md). `retried` is true (`retried=1` on the compact
line) when the first worker chosen closed its connection before reading
the request, as a worker that is stopping does, and the request was sent
to another. `handoff` is `asgi` or `wsgi` (`handoff=` on the compact
line) when the worker [handed the request](handoff.md) to that interface,
and null otherwise. `websocket` is null except on a WebSocket's
closing line, where it holds `event` (`closed`), `by`, `code`, `close`
(the outcome the [metrics](metrics.md) name), `received`, `sent` and
`received_bytes`; that line's `total_ms` is how long the socket was open
and its `bytes` what was sent to the client. The compact line shows the group, the mount and
the class (as `class=`) only when they are not `default`.

## Times on log lines

Every line gets a time when it is written, and where the line goes
decides whether the time is shown:

| Where lines go | Application and Muru lines | Access log lines |
| --- | --- | --- |
| A terminal | no time | your local clock, `05:42:22.836` |
| A pipe, such as a container runtime or a supervisor collecting the output | no time, since the collector stamps each line itself | the full UTC time |
| A file: the daemon's log directory, or output redirected with `>` | the full UTC time | the `time` field of each JSON line |

The full time is RFC 3339 in UTC with milliseconds,
`2026-11-02T05:42:22.836Z`, which sorts as text and reads the same on
every machine. A JSON record carries it in its `time` field, never in
front of the line.

`--timestamps` on `muructl run` or `muructl daemon run` puts the full time
in front of every line on the terminal or the pipe too, for a supervisor
that adds none:

```
$ muructl run app:application --timestamps
2026-11-02T05:42:22.410Z muru: serving app:application on http://127.0.0.1:8080 with 1 worker of 4 threads
2026-11-02T05:42:22.836Z muru.worker[0]: WARNING demo: warned
2026-11-02T05:42:22.840Z 200 GET /hello 18B 0.7ms queue=0.2ms app=0.3ms worker=0 id=01M5P6WZBK740HFT2N80MAV4YV
```

`timestamps = true` in the project's [run table](run-settings.md) does the
same for every run of a checkout, and `--no-timestamps` turns it off for
one. For the daemon, `timestamps = true` in its `[logs]` table does it,
read when the daemon starts. Files carry the time whatever is set.

An access line's time is when the request arrived, and any other line's
is when Muru received it, so the access line of a slow request can carry
an earlier time than lines written before it. An application that puts
`%(asctime)s` in its own logging format gets two times in a file.

## Under the daemon

The daemon writes everything to its log directory (`~/.local/state/muru/logs`
on Linux, `~/Library/Logs/muru` on macOS, `/var/log/muru` in system mode,
or `--log-dir`):

| File | Holds |
| --- | --- |
| `apps/APP/RELEASE.log` | the release's host output and its workers' lines, tagged as above |
| `access.log` | the access log as JSON lines |
| `edge.log` | the edge process's own output |
| `daemon.log` | the daemon's output when started with `muructl daemon start` |

Every line in these files starts with the full UTC time.

`muructl logs APP` shows the last hundred lines of the application's
current release, `muructl logs APP@RELEASE` a particular release's, and
`muructl logs --access` the access log as compact lines (`--json` for the
records as written). `--lines N` changes how many lines come first and
`--follow` keeps printing as lines arrive, through a rotation of the file:

```
$ muructl logs shop --follow
2026-11-02T05:42:22.840Z muru.host[4242]: release ready: 2 workers listening
2026-11-02T05:42:23.102Z muru.worker[0].stdout: shop: served /catalogue
$ muructl logs --access --lines 1
2026-11-02T05:42:23.101Z 200 GET /catalogue 118B 1.2ms release=shop@r1 host=127.0.0.1 queue=0.1ms app=0.9ms worker=1 id=01M5P6WZBK740HFT2N80MAV4YV
```

`--no-timestamps` leaves the times off. The same command against a
development run shows the run's lines with their times as well.

### What the daemon's access log writes

The daemon's access log writes a line for every request unless its
`[logs]` table says otherwise, with the keys `muructl serve` takes:

```toml
[logs]
access_log = 'errors'                            # on (the default), errors or off
slow = '500ms'                                   # what errors counts as slow, 1s by default
access_log_exclude = ['/healthz', '/static/*']
```

`errors` writes 4xx and 5xx responses, requests at least `slow`, and
WebSockets that closed other than normally or going away, and `off`
writes nothing. A request that is not written is still counted in the
[metrics](metrics.md) and shown among the dashboard's recent requests;
`muructl logs --access` reads the file, so it shows only what was
written.

An application's table in an applied file can choose for that
application's requests alone, for example to keep one noisy application
quiet while the rest are logged:

```toml
[app.shop]
access_log = 'off'
```

The application's `access_log` and `slow` replace the `[logs]` table's
for its requests, and its `access_log_exclude` paths are left out as
well as the table's. `muructl daemon reload` applies a changed `[logs]`
table, and `muructl apply` a changed application table, to the running
edge without restarting anything: every request completed from then on
is written by the new choice.

## Rotation

The daemon rotates its log files itself: the access log, the edge's own
log, each release's log and, when it was started in the background, its
own. A file is rotated when the period changes (daily, at midnight UTC,
by default) or when it reaches `size`, if one is set: the daemon renames
it with a timestamp (`access.log.20261025-000000`), has every writer open
its file again by name, compresses the archive in the background
(`.zst` by default) and keeps the newest `keep` archives. A
`logs.rotated` event names what was rotated.

```toml
[logs]
every = 'daily'          # off, hourly, daily or weekly, at the period's start in UTC
size = '100MB'           # also rotate a file that reaches this
keep = 14                # archives kept per log
compress = 'zstd'        # zstd, gzip or off
```

The table is read when the daemon starts and again on `muructl daemon
reload` (or SIGHUP): the daemon rotates on the new policy from its next
check, and a changed period rotates each file once, as a period's end
does. The [daemon guide](daemon.md#reloading-the-configuration) lists
what else a reload applies.

`rotate = 'external'` turns the built-in rotation off for logrotate, or
anything else that renames the files and then runs `muructl logs reopen`,
which makes every writer open its file again by name, so lines go to the
new file and the renamed one is complete. Sending SIGUSR1 to the daemon
does the same. `muructl logs reopen --workers` also runs the
`on_reopen_logs` hook in every worker, for log files the application
opened itself, which is where a `logging` file handler is reopened. A
logrotate entry for a user-mode daemon:

```
/home/app/.local/state/muru/logs/*.log /home/app/.local/state/muru/logs/apps/*/*.log {
    daily
    rotate 14
    compress
    delaycompress
    missingok
    notifempty
    sharedscripts
    postrotate
        muructl logs reopen --workers
    endscript
}
```

Each process reopens its own file: the edge the access log, each host its
release's file (SIGUSR1 to the host does that alone, SIGUSR2 passes SIGUSR1
on to its workers for the hook), and a background daemon its own log.
Lines written between the rename and the reopen land in the renamed file,
and none are lost. In development mode only the access log can go to a
file, and SIGUSR1 to a worker runs the hook.

The instances of a [managed HTTP server](managed-servers.md) write to
their release's file themselves and cannot be told to reopen it, so the
built-in rotation copies that file to its archive and truncates it in
place, and lines written while it is copied are lost. With
`rotate = 'external'`, give those files logrotate's `copytruncate`.
