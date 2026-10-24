# Timeouts

```{admonition} Status
:class: note

Development mode. The soft and hard timeouts that act on the application
itself (section 7.6 of the design specification) arrive in phases 2 and 3;
today's timeouts are the edge's.
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
running in the worker after the client has been answered.

The body size limit is a limit rather than a timeout, but lives beside
them: `--max-body-size 100MB` (the development default; 10 MB with
`--allow-public`), with 413 `body_too_large` when it is passed.
