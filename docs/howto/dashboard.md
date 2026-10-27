# The dashboard

```{admonition} Status
:class: note

A development run and the daemon each serve a dashboard on a loopback
address with charts over the last hour or day. The daemon's needs a
signed-in session, serves the same figures as JSON, with rollouts,
scaling and maintenance, streams its history and events with
Server-Sent Events, and can turn maintenance mode on and off for a
session that may act. Worker inspection views and the other actions are
still to come.
```

## In development mode

`muructl run` serves a dashboard at `http://127.0.0.1:8081/` with the
workers, requests per second and latency over the last minute, charts
of requests, errors and latency over time, recent requests and the
host's and workers' output, refreshed every two seconds, with no
sign-in. `--dashboard ADDRESS` moves it to another loopback address and
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

`muructl status` shows the address.

### Signing in

Every page needs a session. `muructl dashboard login` asks the daemon,
over the admin socket, for a link:

```console
$ muructl dashboard login
http://127.0.0.1:8090/login?code=3f9c...
muru: open the link within 5m to sign in; it works once
```

Opening the link gives the browser a session for twelve hours, so
whoever may use the admin socket, and only they, may sign in. A link
works once and only within five minutes, and restarting the daemon ends
every session. Opening the dashboard without a session shows how to sign
in.

When the dashboard is reached at another address, through an SSH tunnel
or a proxy that serves it under a path, `--url` prints the link under
that address instead, or the `MURU_DASHBOARD_URL` environment variable
gives it:

```console
$ muructl dashboard login --url https://example.com/proxy/8090/
https://example.com/proxy/8090/login?code=3f9c...
```

The dashboard's pages use relative addresses, so they work under any
path, and open it with the trailing slash: `https://example.com/proxy/8090/`.

### What the page shows

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

### JSON for scripts

The same figures are served as JSON, each path answering with what the
page shows. A script signs in with a token rather than a link:

```console
$ muructl dashboard login --token
5be0...
$ curl -H "Authorization: Bearer 5be0..." http://127.0.0.1:8090/v1/apps
```

A token lasts twelve hours, or until the daemon restarts.

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
| `/metrics` | the Prometheus exposition, which needs no session |

A development run serves `/v1/history` and `/v1/history/stream` too.
The history is computed from the edge's own counters once a second (the
daemon asks its edge for them), so rates are exact and the percentiles
come from the request histogram; a day's points fold each minute's
seconds into mean rates and the worst percentiles.

The pages are served with a strict content security policy, and the
address is loopback, so reaching the dashboard from elsewhere means an
SSH tunnel or a proxy of your own in front of it. A page from the same
origin may show the dashboard in a frame, so behind jupyter-server-proxy
it opens in a JupyterLab pane; pages from other sites may not frame it.
The charts are drawn by uPlot and the live updates by the htmx
Server-Sent Events extension, both vendored into the binary.

## Actions

A session may look unless it was asked for with `--actions`, which the
daemon allows only when its `[dashboard]` table turns actions on:

```toml
[dashboard]
listen = "127.0.0.1:8090"
actions = true
```

```console
$ muructl dashboard login --actions
```

A session that may act shows a button beside each application's
serving release that turns [maintenance mode](error-pages.md#maintenance-mode)
on or off. Deploys, rollbacks, drains and scaling stay with `muructl`.

Each action is a form posted from the dashboard's own page, carrying a
token of the session's own, so another site cannot act through a
signed-in browser. A script with a token from `--actions --token` posts
the same form fields, `app` and `state` (`on` or `off`), to
`/actions/maintenance`. Every action is recorded as a `dashboard.action`
event naming the start of the session's id, beside the event the action
itself raises, such as `maintenance.changed`, and every link or token
issued is a `dashboard.login_issued` event.
