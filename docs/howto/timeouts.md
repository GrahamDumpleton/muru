# Timeouts

```{admonition} Status
:class: note

The edge's timeouts, the soft timeout, which interrupts a request that
runs too long, the hard timeout, which replaces a worker whose request
never returns, and the loop stall timeout, which replaces an ASGI worker
whose event loop is held.
```

Every timeout has a name, a default, and a response the client sees when
it fires. Change one with `--timeout NAME=DURATION`, repeatable, where a
duration is `250ms`, `2s` or a bare number of seconds:

```
muructl run app:application --timeout deadline=30s --timeout queue=2s
```

| Name | Default | Measured | When it fires |
| --- | --- | --- | --- |
| `header_read` | 30s | from the connection's first byte to a complete request head | the connection is closed |
| `queue` | 5s | from arrival to a worker thread picking the request up | 503 `queue_full` with `Retry-After: 1` |
| `deadline` | 300s | from arrival to the response head | 504 `deadline`; the worker is told to abandon the request |
| `body_idle` | 60s | between two chunks of a streamed request body | 408 `client_timeout` if no response has started |
| `response_idle` | 60s | between two chunks of a streamed response | the response is cut off and the connection closed |
| `keep_alive` | 20s | an idle keep-alive connection waiting for its next request | the connection is closed (see the [connections guide](connections.md)) |
| `keep_alive_proxy` | 75s | the same, for a trusted proxy's connection | the connection is closed |

`queue` is the one to tune when the application is overloaded: a short
queue timeout turns a backlog into fast 503s with a `Retry-After`, which
load balancers and clients handle better than slow successes. `deadline`
bounds the slowest acceptable request; the access log shows `app=` time per
request, so the right value is the slowest legitimate one plus margin.

A request past its deadline keeps running in the worker after the client
has been answered, unless a soft timeout interrupts it or a hard timeout
replaces its worker. Both are off by default. The application can see
the deadline coming: `environ["muru.deadline"]`, or `deadline` in an
ASGI scope's `extensions["muru"]`, is when the edge gives up, in seconds
since the epoch, so a handler can skip work it has no time left for:

```python
import time

def application(environ, start_response):
    if environ["muru.deadline"] - time.time() < 2:
        ...  # answer from the cache instead
```

## The soft timeout

The soft timeout interrupts a request where it is. A watchdog in each
worker checks its requests in flight every 100 ms and, when one has run
past the timeout, raises `muru.RequestTimeout` in the thread serving it,
or cancels the task of an ASGI request. The exception is raised at the
next line of Python the thread runs: a thread waiting in `wsgi.input`
for the request body is woken at once, and one inside a C call, such as
a database driver waiting for a row, sees it when the call returns. The
worker writes the traceback to its log, which shows where the request
was when it was interrupted, and answers 504 `soft_timeout` unless the
response had already started, in which case the response is cut off.
The worker goes on serving.

`muru.RequestTimeout` is a `BaseException`, not an `Exception`, so a
handler that catches `Exception` to log and carry on does not swallow
it. An application that wants to answer for itself catches it by name:

```python
import muru

def application(environ, start_response):
    try:
        rows = run_report()
    except muru.RequestTimeout:
        start_response("503 Service Unavailable", [("Content-Type", "text/plain")])
        return [b"the report took too long; try a narrower date range\n"]
    ...
```

For an ASGI application the task is cancelled, so the application sees
`asyncio.CancelledError` at its next `await`, and its `finally` blocks
and context managers run as they do for any cancellation.

| Name | Default | Measured | When it fires |
| --- | --- | --- | --- |
| `soft` | off | from the request reaching the worker | the request is interrupted and answered 504 `soft_timeout` |

```
muructl run app:application --timeout soft=30s
muructl deploy shop /srv/shop --timeout soft=30s --timeout hard=60s
```

Streaming responses and WebSockets are exempt: once a response head goes
out with `Content-Type: text/event-stream` or `X-Accel-Buffering: no`,
or a WebSocket is accepted, the request runs as long as it needs to and
counts toward neither the soft nor the hard timeout. The
[connections guide](connections.md) covers the idle timeouts that bound
those instead.

## The hard timeout

The host watches its workers, each of which reports once a second how
long its oldest request has been running. A request past the hard
timeout is treated as stuck: the worker is taken out of routing at once,
every thread's stack is written to the log (with `faulthandler`, which
needs no cooperation from the stuck thread), a fresh worker is forked on
a new socket and the edge routes to it as soon as it listens. The stuck
worker gets `hard_drain_grace` to finish its other requests, is then told
to stop, and is killed `shutdown_timeout` later if it has not. The stuck
request's client is answered by the edge when its connection to the
worker closes, and a `worker.timeout_killed` event records what happened.

| Name | Default | Measured | When it fires |
| --- | --- | --- | --- |
| `hard` | off | from the request reaching the worker | the worker is replaced |
| `hard_drain_grace` | 10s | from the worker leaving routing | the worker is told to stop |
| `websocket_drain_grace` | 0s | from the worker being told to stop | its open WebSockets are closed with 1001 |
| `shutdown` | 5s | from the worker's WebSockets being closed | the worker is killed |

```
muructl run app:application --timeout hard=60s
muructl deploy shop /srv/shop --timeout hard=60s --timeout hard_drain_grace=5s
```

In a project's `[tool.muru]` table or a declared release they are the
`timeouts` table, written as durations:

```toml
[tool.muru.timeouts]
soft_timeout = "30s"
hard_timeout = "60s"
hard_drain_grace = "10s"
websocket_drain_grace = "0s"
shutdown_timeout = "5s"
loop_stall_timeout = "10s"
```

Set the hard timeout above the edge's `deadline`, so that a slow request
is answered 504 by the edge first and only a request that never returns
costs a worker, and at least 5 seconds above the soft timeout, which
`muructl` and `muructl apply` check: the hard timeout is the backstop for
a request the soft timeout could not interrupt, such as one stuck in a C
call that never returns. The [worker recycling guide](recycling.md)
describes the replacement itself, which is the same path a recycled
worker takes.

## Stopping a worker

Every worker stops the same way, whether its release is drained, rolled
back from or stopped, or the worker is recycled, scaled in or replaced
after a hard timeout. Told to stop, it takes no new requests and runs
the `on_drain` hook. Its open WebSockets keep running for
`websocket_drain_grace`, then the worker closes each with 1001 ("the
server is going away"): the application's handler gets
`websocket.disconnect` with 1001, the client gets the same code and can
reconnect to the release that serves, and the close is counted as
`drained`. What is still running then gets `shutdown_timeout` before the
host kills the worker, so a worker is gone at most
`websocket_drain_grace` plus `shutdown_timeout` after it was told to stop.

A grace longer than nothing lets a socket finish what it is doing, such
as a message being sent; most clients reconnect on 1001 at once, so the
default closes sockets as soon as the worker is told to stop.

## The loop stall timeout

An ASGI application's requests share one event loop in each worker, so
code that blocks without awaiting, a `time.sleep` or a synchronous
database call in an `async def`, holds every request in the worker. The
loop beats twice a second, and a watchdog outside Python notices when it
has not beaten for the loop stall timeout. The worker is then replaced
as for the hard timeout, stacks and all, with a `worker.loop_stalled`
event before the `worker.timeout_killed` one.

| Name | Default | Measured | When it fires |
| --- | --- | --- | --- |
| `loop_stall` | 10s | since the event loop last ran | the worker is replaced |

```
muructl run app:application --timeout loop_stall=5s
```

A worker being debugged is pinned, so a `breakpoint()` in a coroutine,
which holds the loop while it waits, does not cost the worker.

The body size limit is a limit rather than a timeout, but lives beside
them: `--max-body-size 100MB` (the development default; 10 MB with
`--allow-public`), with 413 `body_too_large` when it is passed.
