# Timeouts

```{admonition} Status
:class: note

The edge's timeouts and the hard timeout, which replaces a worker whose
request runs too long. The soft timeout, which interrupts the request
itself, and the loop stall timeout are still to come.
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

`queue` is the one to tune when the application is overloaded: a short
queue timeout turns a backlog into fast 503s with a `Retry-After`, which
load balancers and clients handle better than slow successes. `deadline`
bounds the slowest acceptable request; the access log shows `app=` time per
request, so the right value is the slowest legitimate one plus margin.

The deadline is also passed to the application: `muru.deadline` in the
environ is planned, and in phase 3 a soft timeout can interrupt the
application thread itself. Until then a request past its deadline keeps
running in the worker after the client has been answered, which is what
the hard timeout is for.

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
| `shutdown` | 5s | from the worker being told to stop | the worker is killed |

```
muructl run app:application --timeout hard=60s
muructl deploy shop /srv/shop --timeout hard=60s --timeout hard_drain_grace=5s
```

In a project's `[tool.muru]` table or a declared release they are the
`timeouts` table, written as durations:

```toml
[tool.muru.timeouts]
hard_timeout = "60s"
hard_drain_grace = "10s"
shutdown_timeout = "5s"
```

Set the hard timeout above the edge's `deadline`, so that a slow request
is answered 504 by the edge first and only a request that never returns
costs a worker. The [worker recycling guide](recycling.md) describes the
replacement itself, which is the same path a recycled worker takes.

The body size limit is a limit rather than a timeout, but lives beside
them: `--max-body-size 100MB` (the development default; 10 MB with
`--allow-public`), with 413 `body_too_large` when it is passed.
