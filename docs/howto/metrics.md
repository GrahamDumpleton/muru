# Metrics

```{admonition} Status
:class: note

The daemon serves request counters and histograms, slot and queue gauges,
the workers ready, each worker's requests, memory, CPU time, GIL waits and
event loop lag, the workers recycled and exited, the autoscaler's decisions
and reaps, and each listener's connections and TLS handshakes in the
Prometheus text format, on a metrics listener and through `muructl
metrics`; a development run serves the request and connection figures from
its dashboard. OTLP export is still to come.
```

Muru counts what the edge does with each request and what the daemon
knows about the workers, and serves the figures in the Prometheus text
format at `/metrics`.

## Serving them

Under the daemon, a `[metrics]` table in the daemon's configuration file
names the address, or `--metrics` on `muructl daemon start` does:

```toml
[metrics]
listen = "127.0.0.1:9464"
```

Nothing is served unless one of them says so, and the address is meant to
be an internal one: the exposition is never on the application's
listeners. `muructl status` shows the address, and `muructl metrics`
prints the same text for a look without a scraper. A Prometheus scrape
configuration for it:

```yaml
scrape_configs:
  - job_name: muru
    static_configs:
      - targets: ["127.0.0.1:9464"]
```

The [dashboard](dashboard.md) serves `/metrics` beside its pages too: in
development mode at `http://127.0.0.1:8081/metrics` unless `--dashboard`
says otherwise, and under the daemon on the dashboard's address.

## What is reported

Names start with `muru_`. Where they apply, labels name the application
(`app`), the release (`release`), the worker group (`group`, which is
`default` or a named [worker group](worker-groups.md)) and the mount
(`mount`, `default` or a named [mount](mounts.md)).

| Metric | Kind | Labels | Holds |
| --- | --- | --- | --- |
| `muru_requests_total` | counter | app, release, group, mount, status, reason, client_class | requests completed; `status` is the class (`2xx`), `reason` is present when Muru generated the response, with the values the [error responses](../reference/errors.md) page lists, and `client_class` is the request's [class](client-classes.md) |
| `muru_request_duration_seconds` | histogram | app, release, phase | time in the `queue` (waiting for a worker slot), in the `application` (until the response head arrived) and in `total`; the first two only for requests a worker served |
| `muru_request_retries_total` | counter | app, release, group | requests sent to a second worker after the first could not be connected to or closed its connection before reading them, as a worker that is stopping does; such a request is marked `retried` in the [access log](logging.md) |
| `muru_requests_in_flight` | gauge | app, release | requests taken and not yet answered; a WebSocket counts until the application accepts it |
| `muru_websocket_connections` | gauge | app, release, group, mount | WebSockets open |
| `muru_websockets_closed_total` | counter | app, release, group, mount, by, close | WebSockets closed: `by` is the side that closed first (`client`, `application`, or `edge` for Muru itself) and `close` how, one of `normal`, `going_away`, `idle_timeout`, `drained` (its worker was stopping), `abnormal` (gone without a close frame), `too_large`, `protocol_error`, `server_error` or `other` |
| `muru_websocket_duration_seconds` | histogram | app, release, group | how long WebSockets stayed open, in buckets from a second to a day |
| `muru_rate_limited_total` | counter | limit, client_class, action | requests a [rate limit](client-classes.md) refused (`rejected`), held (`delayed`) or only counted (`logged`) |
| `muru_upload_spool_bytes` | gauge | app | bytes the edge holds for each application's [uploads](uploads.md) in progress |
| `muru_auth_requests_total` | counter | result | [authentication sub-requests](auth-subrequests.md) by result: `allowed`, `cached`, `denied` or `unavailable` |
| `muru_queue_depth` | gauge | app, release, group | requests waiting for a slot |
| `muru_slots_busy` | gauge | app, release, group | slots handling a request or holding an open WebSocket |
| `muru_slots_total` | gauge | app, release, group | slots across the workers: threads, or the event loop's concurrency |
| `muru_async_slots_busy` | gauge | app, release, group | requests held on the event loops of [ASGI](asgi.md) workers, the part of `muru_slots_busy` that is not on threads |
| `muru_upstream_health` | gauge | upstream, address | 1 for an [upstream](upstreams.md) address in service, 0 for one its probes found unhealthy or that was ejected after failed requests |
| `muru_workers_ready` | gauge | app, release, group | workers listening for requests (under the daemon) |
| `muru_workers_desired` | gauge | app, release, group | workers the [autoscaler](scaling.md) is bringing the group to |
| `muru_scaling_decisions_total` | counter | app, release, group, direction | autoscaler decisions, `out` or `in` |
| `muru_churn_reaps_total` | counter | app, release, group | workers the autoscaler reaped in the last hour, against its churn budget |
| `muru_worker_requests_total` | counter | app, release, group, worker | requests a worker has served, as it last reported (under the daemon) |
| `muru_worker_requests_in_flight` | gauge | app, release, group, worker | requests a worker was serving when it last reported |
| `muru_worker_cpu_seconds_total` | counter | app, release, group, worker | CPU time a worker has used, user and system, as it last reported every `check_interval` of its memory policy |
| `muru_worker_memory_bytes` | gauge | app, release, group, worker, measure | a worker's memory as `rss`, `pss` or `uss`, measured every `check_interval` of its [memory policy](memory.md) |
| `muru_gil_wait_seconds` | histogram | app, release, group, worker | how long a worker's threads waited for the GIL when they had work to run, in buckets from 0.1 ms to 5 s, cumulative since the worker started and reported with its memory; absent until the worker has waited at all, so a free-threaded worker never reports it |
| `muru_loop_lag_seconds` | histogram | app, release, group, worker | how much later than due an ASGI worker's event loop heartbeat ran, which beats twice a second, in the same buckets; absent for a worker without a loop |
| `muru_worker_recycles_total` | counter | app, release, group, reason | workers their host replaced since the release started: `max_requests`, `max_lifetime`, `memory` ([recycling](recycling.md) and the [memory policy](memory.md)), `hard_timeout`, `loop_stalled` or `gil_stalled` ([timeouts](timeouts.md)) |
| `muru_worker_exits_total` | counter | app, release, group, reason | workers that exited since the release started: `scaled_in` by the autoscaler, `crashed` (exited on their own and were restarted) or `start_failed` (a replacement or a worker of an optional group that never became ready) |
| `muru_connections_open` | gauge | listener | connections open on a listener |
| `muru_connections_refused_total` | counter | listener, reason | connections a listener refused: `client_limit`, `overloaded` (answered 503) or `paused` (closed), see the [connections guide](connections.md) |
| `muru_tls_handshakes_total` | counter | listener, result | [TLS](https.md) handshakes a listener began: `completed`, `failed` (the client refused the certificate, or offered nothing the listener speaks) or `timed_out` (the client never finished within the header read timeout) |
| `muru_snapshot_version` | gauge | | the routing snapshot in force at the edge, 0 in development mode |

A request answered before any release serves, or refused by the routing
tables, carries no application or release labels.

Some figures are read from these families rather than exported on their
own: the time requests wait for a worker is the `queue` phase of
`muru_request_duration_seconds`, a worker's CPU as a share of a core is
`rate(muru_worker_cpu_seconds_total[5m])`, and requests refused for the
size of their body are `muru_requests_total` with the `body_too_large`
reason. The worker families carry each worker by its index, so a worker
replaced under [recycling](recycling.md) starts its counters and
histograms again, as `rate()` and `increase()` expect of a restarted
target.

```
# HELP muru_requests_total Requests completed, by status class and the reason when the edge generated the response.
# TYPE muru_requests_total counter
muru_requests_total{app="shop",release="r1",group="default",mount="default",status="2xx",client_class="default"} 1284
muru_requests_total{app="shop",release="r1",group="default",mount="default",status="5xx",reason="app_error",client_class="default"} 3
# HELP muru_request_duration_seconds Time a request spent waiting for a worker (queue), in the application, and in total.
# TYPE muru_request_duration_seconds histogram
muru_request_duration_seconds_bucket{app="shop",release="r1",phase="total",le="0.005"} 1180
```

A WebSocket is a request until the application accepts it: it is
counted in `muru_requests_total` with a `1xx` status, and the handshake's
time goes into `muru_request_duration_seconds`. From then on it is an
open WebSocket, counted in its own gauge, and its lifetime goes into
`muru_websocket_duration_seconds` when it closes, so long-lived sockets
never stretch the request percentiles or the [rollout](rollouts.md)
gates.

The request histogram's buckets run from 1ms to 10s. Counters start from zero when
an edge process starts, so a daemon restart keeps them and an edge
replacement resets them, which `rate()` takes in its stride. A release
that has drained and stopped keeps its counters for as long as the edge
runs.
