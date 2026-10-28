# The dashboard

```{admonition} Status
:class: note

A development run and the daemon each serve a dashboard on a loopback
address with charts over the last hour or day. The daemon's needs a
signed-in session, serves the same figures as JSON, with rollouts,
scaling and maintenance, streams its history and events with
Server-Sent Events, and can turn maintenance mode on and off for a
session that may act, which can also promote, roll back and drain
releases, pin a group's workers and restart a worker, and shows each
running release's workers with their status, stacks, tasks and memory.
```

## In development mode

`muructl run` serves a dashboard at `http://127.0.0.1:8081/` with the
workers, requests per second and latency over the last minute, the
WebSockets open, charts of requests, errors, latency and what is open
over time, recent requests and the host's and workers' output, refreshed every two seconds, with no
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
  total time, 5xx responses, requests in flight and WebSockets open;
- every application's releases with their state, whether traffic goes to
  them, the workers listening, the slots busy (WebSockets hold one each),
  the WebSockets open and the slots in all, and requests waiting for a
  slot, with when maintenance mode began for an application
  in it, and the virtual hosts and routes when declared;
- the [rollouts](rollouts.md) under way and the latest that ended, with
  the candidate's share and what each is doing or why it ended;
- each running release's worker groups as the [autoscaler](scaling.md)
  sees them: workers, policy, the workers' CPU, GIL wait and event loop
  lag, the diagnosis with the tuning it suggests, and the last decision;
- the [WebSockets](asgi.md#watching-websockets) open, counted by endpoint
  and the longest open listed;
- charts of requests and errors per second, of the median and 95th
  percentile total time, and of requests in flight and WebSockets open, over the last hour at one second or, with
  `?range=24h`, the last day at one minute;
- recent requests across every release, the last fifty events (new ones
  arriving as they happen), and the tail of each serving release's log.

### Looking into the workers

Each running release on the page links to its workers, where the
dashboard shows what [`muructl debug`](debugging.md) shows:

- the release's workers as `debug status` lists them: pid, group,
  uptime, requests served and in flight, the age of the oldest request,
  memory, and whether each is ready, starting, being replaced or being
  debugged, refreshed every two seconds;
- their stacks as `debug stacks` shows them: the Python stack of every
  thread with the request each is serving, threads showing the same
  stack across workers shown once, taken when the page opens and again
  on request.

Each worker links to a page of its own with its status, its stacks, its
asyncio tasks as `debug tasks` shows them, and, when asked for, its
memory as `debug memory` shows it: resident, proportional and unique
size, the garbage collector's counts, and the object types it holds most
of. Counting the objects holds the interpreter while it runs, so the
page takes it only on request, and it is recorded as a `debug.memory`
event as the command is.

A page is at `workers?release=APP@RELEASE` and `worker?release=APP@RELEASE&worker=N`
under the dashboard's address; a release that is not running says so
in place of its workers.

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
| `/v1/websockets` | the WebSockets open: `open`, `endpoints` counted by path pattern, and `sockets`, the longest open first |
| `/v1/events` | recent events, newest first |
| `/v1/debug/OP?release=APP@RELEASE` | a release's workers' answers to `status`, `stacks`, `tasks` or `memory`, as `muructl debug OP --json` prints them, one worker with `&worker=N`; 409 when the release is not running |
| `/v1/history?range=1h` | the charts' points: `[second, requests/s, errors/s, p50 ms, p95 ms, in flight, workers, WebSockets open]`, one a second for the last hour, or one a minute for the last day with `range=24h` |
| `/v1/history/stream` | each new point as a Server-Sent Event of type `point` |
| `/v1/events/stream` | each new event as it happens, as a rendered table row (type `event`) and as JSON (type `json`) |
| `/metrics` | the Prometheus exposition, which needs no session |

A development run serves `/v1/history`, `/v1/history/stream` and
`/v1/websockets` too.
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

A session that may act shows buttons beside each release and worker
group, and each worker, for what `muructl` would do:

- beside the serving release, turning [maintenance
  mode](error-pages.md#maintenance-mode) on or off, and rolling back to
  the previous release when there is one;
- beside a ready release that is not serving, promoting it, as after a
  held deploy or a rollout;
- beside any running release, draining it, which takes it out of
  traffic when it serves and stops it;
- beside each worker group in the scaling table, pinning its workers to
  a count, and unpinning a pinned group so the autoscaler decides again;
- beside each ready worker on the [workers
  pages](#looking-into-the-workers), restarting it, with `force` to kill
  it at once as `muructl restart --force` does.

Deploys and rollouts stay with `muructl`, and a release an overlay
freezes is not promoted from the page.

Each action is a form posted from the dashboard's own page, carrying a
token of the session's own, so another site cannot act through a
signed-in browser. A script with a token from `--actions --token` posts
the same form fields to the action's path:

| Path | Fields |
| --- | --- |
| `/actions/maintenance` | `app`, `state` (`on` or `off`) |
| `/actions/promote` | `app`, `release` |
| `/actions/rollback` | `app` |
| `/actions/drain` | `app`, `release` |
| `/actions/scale` | `app`, `release`, `group`, and `processes` to pin or `unpin=on` |
| `/actions/restart` | `app`, `release`, `worker`, and `force=on` to kill it at once |

The answer is 303 back to the page, 400 for a form that is not an
action, or 409 with the reason the daemon refused it, as `muructl`
would print. Every action is recorded as a `dashboard.action` event
naming the start of the session's id, beside the events the action
itself raises, such as `maintenance.changed` or `worker.restarted`, and
every link or token issued is a `dashboard.login_issued` event.
