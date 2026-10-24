# Metrics

```{admonition} Status
:class: note

The daemon serves request counters and histograms, slot and queue gauges
and the workers ready in the Prometheus text format, on a metrics
listener and through `muructl metrics`, and a development run serves the
same from its dashboard. Worker CPU and memory, scaling, connection and
TLS metrics, and OTLP export are still to come.
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
| `muru_requests_total` | counter | app, release, group, mount, status, reason | requests completed; `status` is the class (`2xx`) and `reason` is present when Muru generated the response, with the values the [error responses](../reference/errors.md) page lists |
| `muru_request_duration_seconds` | histogram | app, release, phase | time in the `queue` (waiting for a worker slot), in the `application` (until the response head arrived) and in `total`; the first two only for requests a worker served |
| `muru_requests_in_flight` | gauge | app, release | requests taken and not yet answered |
| `muru_queue_depth` | gauge | app, release, group | requests waiting for a slot |
| `muru_slots_busy` | gauge | app, release, group | slots handling a request |
| `muru_slots_total` | gauge | app, release, group | slots across the workers: threads, or the event loop's concurrency |
| `muru_workers_ready` | gauge | app, release, group | workers listening for requests (under the daemon) |
| `muru_snapshot_version` | gauge | | the routing snapshot in force at the edge, 0 in development mode |

A request answered before any release serves, or refused by the routing
tables, carries no application or release labels.

```
# HELP muru_requests_total Requests completed, by status class and the reason when the edge generated the response.
# TYPE muru_requests_total counter
muru_requests_total{app="shop",release="r1",group="default",mount="default",status="2xx"} 1284
muru_requests_total{app="shop",release="r1",group="default",mount="default",status="5xx",reason="app_error"} 3
# HELP muru_request_duration_seconds Time a request spent waiting for a worker (queue), in the application, and in total.
# TYPE muru_request_duration_seconds histogram
muru_request_duration_seconds_bucket{app="shop",release="r1",phase="total",le="0.005"} 1180
```

The histogram buckets run from 1ms to 10s. Counters start from zero when
an edge process starts, so a daemon restart keeps them and an edge
replacement resets them, which `rate()` takes in its stride. A release
that has drained and stopped keeps its counters for as long as the edge
runs.
