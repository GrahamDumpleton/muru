# The dashboard

```{admonition} Status
:class: note

A development run and the daemon each serve a read-only dashboard on a
loopback address with charts over the last hour or day, the daemon's
serves the same figures as JSON, with rollouts, scaling and maintenance,
and streams its history and events with Server-Sent Events. Worker
inspection views and mutating actions are still to come.
```

## In development mode

`muructl run` serves a dashboard at `http://127.0.0.1:8081/` with the
workers, requests per second and latency over the last minute, charts
of requests, errors and latency over time, recent requests and the
host's and workers' output, refreshed every two seconds.
`--dashboard ADDRESS` moves it to another loopback address and
`--dashboard off` turns it off; with `--allow-public` it is off unless
asked for. `/metrics` on the same address is the Prometheus exposition
the [metrics guide](metrics.md) describes.

## Under the daemon

The daemon serves its dashboard when `--dashboard ADDRESS` on `muructl
daemon start` or a `[dashboard]` table in the configuration file names a
loopback address; any other address is refused:

```toml
[dashboard]
listen = "127.0.0.1:8090"
```

`muructl status` shows the address. The page has:

- the node: the daemon's pid and version, its uptime, the listeners, the
  edge process and the routing snapshot in force;
- the last minute: requests per second, median and 95th percentile
  total time, 5xx responses and requests in flight;
- every application's releases with their state, whether traffic goes to
  them, the workers listening, the slots busy and in all, and requests
  waiting for a slot, with when maintenance mode began for an application
  in it, and the virtual hosts and routes when declared;
- the [rollouts](rollouts.md) under way and the latest that ended, with
  the candidate's share and what each is doing or why it ended;
- each running release's worker groups as the [autoscaler](scaling.md)
  sees them: workers, policy, the workers' CPU, GIL wait and event loop
  lag, the diagnosis with the tuning it suggests, and the last decision;
- charts of requests and errors per second and of the median and 95th
  percentile total time, over the last hour at one second or, with
  `?range=24h`, the last day at one minute;
- recent requests across every release, the last fifty events (new ones
  arriving as they happen), and the tail of each serving release's log.

The same figures are served as JSON for scripts, each path answering
with what the page shows:

| Path | Holds |
| --- | --- |
| `/v1/status` | the node and the last minute's summary |
| `/v1/apps` | applications with their releases |
| `/v1/apps/APP/releases` | one application's releases |
| `/v1/hosts`, `/v1/routes` | virtual hosts and routes |
| `/v1/rollouts` | rollouts, those under way first, then the latest ten |
| `/v1/scaling` | each running release's worker groups, as `muructl scale status` shows them |
| `/v1/requests` | recent requests, newest first |
| `/v1/events` | recent events, newest first |
| `/v1/history?range=1h` | the charts' points: `[second, requests/s, errors/s, p50 ms, p95 ms, in flight, workers]`, one a second for the last hour, or one a minute for the last day with `range=24h` |
| `/v1/history/stream` | each new point as a Server-Sent Event of type `point` |
| `/v1/events/stream` | each new event as it happens, as a rendered table row (type `event`) and as JSON (type `json`) |
| `/metrics` | the Prometheus exposition |

A development run serves `/v1/history` and `/v1/history/stream` too.
The history is computed from the edge's own counters once a second (the
daemon asks its edge for them), so rates are exact and the percentiles
come from the request histogram; a day's points fold each minute's
seconds into mean rates and the worst percentiles.

Everything is read-only: deploys, rollbacks and drains stay with
`muructl` and the admin socket. The pages are served with a strict
content security policy, and the address is loopback, so reaching the
dashboard from elsewhere means an SSH tunnel or a proxy of your own in
front of it. The charts are drawn by uPlot and the live updates by the
htmx Server-Sent Events extension, both vendored into the binary.
