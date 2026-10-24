# The dashboard

```{admonition} Status
:class: note

A development run and the daemon each serve a read-only dashboard on a
loopback address, and the daemon's serves the same figures as JSON.
Charts over time, Server-Sent Events, worker inspection views and
mutating actions are still to come.
```

## In development mode

`muructl run` serves a dashboard at `http://127.0.0.1:8081/` with the
workers, requests per second and latency over the last minute, recent
requests and the host's and workers' output, refreshed every two seconds.
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
  waiting for a slot, with the virtual hosts and routes when declared;
- recent requests across every release, the last fifty events, and the
  tail of each serving release's log.

The same figures are served as JSON for scripts, each path answering
with what the page shows:

| Path | Holds |
| --- | --- |
| `/v1/status` | the node and the last minute's summary |
| `/v1/apps` | applications with their releases |
| `/v1/apps/APP/releases` | one application's releases |
| `/v1/hosts`, `/v1/routes` | virtual hosts and routes |
| `/v1/requests` | recent requests, newest first |
| `/v1/events` | recent events, newest first |
| `/metrics` | the Prometheus exposition |

Everything is read-only: deploys, rollbacks and drains stay with
`muructl` and the admin socket. The pages are served with a strict
content security policy, and the address is loopback, so reaching the
dashboard from elsewhere means an SSH tunnel or a proxy of your own in
front of it.
