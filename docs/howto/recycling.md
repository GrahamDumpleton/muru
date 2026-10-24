# Worker recycling

```{admonition} Status
:class: note

Workers are replaced after a number of requests or a lifetime, on the
hard timeout and over the memory limits. Per-group settings and the
surge limit are still to come.
```

A worker process that has served long enough is replaced by a fresh one,
without a request lost: the host forks the replacement on a new socket,
tells its supervisor (the daemon, or `muructl run`) about it, waits until
the edge routes to the new worker, and only then tells the old one to
stop, which finishes the requests it was serving and exits. Two settings
say when:

| Setting | Default | Replaces a worker |
| --- | --- | --- |
| `max_requests` | off | after it has served this many requests |
| `max_lifetime` | off | after it has lived this long, for example `1h` |

Both carry jitter of up to ten percent, so the workers of a release do
not all go at once.

```
muructl run app:application --max-requests 1000
muructl deploy shop /srv/shop --max-requests 1000 --max-lifetime 2h
```

```toml
[tool.muru]
max_requests = 1000
max_lifetime = "2h"
```

The same keys go in a declared release for `muructl apply`, and `muructl
run --print-config` writes them. A release records its settings when it
is created, so changing them means a new deploy.

## What happens

Each worker reports to the host once a second: the requests it has
served, the requests in flight and the age of the oldest. When a worker
reaches its count or its lifetime, the host forks a replacement, which
listens on a socket of its own named for its generation, and announces
the new set of worker sockets to the supervisor over the host's control
socket. The daemon pushes a routing snapshot with the new socket to the
edge (and a development run reconnects its pools), acknowledges, and the
host sends the old worker SIGTERM. The old worker stops accepting,
finishes the requests it holds and exits; requests still being dispatched
to it over the edge's existing connections complete normally. If the
supervisor does not answer within five seconds, the old worker is told to
stop regardless.

A worker whose request runs past the [hard timeout](timeouts.md) takes
the same path, except that it leaves routing before its replacement is
ready and its stacks are written to the log first, and so does a worker
over its [memory limits](memory.md).

Events record each replacement: `worker.started` and `worker.ready` for
the new worker, then `worker.recycled` (or `worker.timeout_killed`) with
the reason once the old one has gone, under the release's subject; a
worker that exits on its own is `worker.exited`. The daemon restarts a
release whose host dies, and a host that cannot start a replacement
three times running keeps the old worker and stops trying.
