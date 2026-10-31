# Worker recycling

```{admonition} Status
:class: note

Workers are replaced after a number of requests or a lifetime, on the
hard timeout and over the memory limits, with the memory policy and
timeouts settable per [worker group](worker-groups.md). Request and
lifetime limits per group and the surge limit are still to come.
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
the new worker, then `worker.recycled` (or `worker.timeout_killed`, or
`worker.restarted`) with the reason once the old one has gone, under the
release's subject; a worker that exits on its own is `worker.exited`.
The [metrics](metrics.md) count them too, as `muru_worker_recycles_total`
by reason and `muru_worker_exits_total`. The daemon restarts a release
whose host dies, and a host that cannot start a replacement three times
running keeps the old worker and stops trying.

## Restarting a worker by hand

`muructl restart` replaces one worker of a release the way a recycle
does, for a fault the policies do not see: a broken connection pool,
bad state left in the process, or a request that hangs with the hard
timeout off. The worker is named by the index `muructl debug status`
and the access log's `worker=` field show, or by its process id:

```
muructl restart shop --worker 1
muructl restart shop@r3 --pid 48213
muructl restart shop --all
muructl restart shop --all --group reports
```

The replacement is forked on a new socket, the edge routes to it, and
only then is the old worker told to stop, so no request is lost; the
command waits until the new worker serves and the old one has gone, and
prints both pids (`--no-wait` returns as soon as the restart is under
way). `--all` restarts every worker in turn, one at a time, so capacity
holds. A worker that is frozen is recovered the same way, since the host
acts on its own records and signals and never needs the worker to
answer; it takes about ten seconds with the default timeouts.

`--force` is for a worker that must go now: it leaves routing at once,
its threads' stacks are written to the log as they are for the hard
timeout, and it is killed a second later without being asked to stop.
The requests it was serving fail with 502, and a debugger session in it
ends. Without `--force`, a worker being debugged is left alone and the
command says so.

```
muructl restart shop --worker 1 --force
```

A restart resets the worker's state but loads no new code: workers fork
from the host's already loaded interpreter, so the application is as it
was when the release started. New code is a new release (`muructl
deploy`), and in development `muructl run` reloads on its own when a
source file changes. The event is `worker.restarted`, with the reason and, for
`--force`, where the stacks were; manual restarts do not count against
the churn budget. The command works against a development run too,
through the run's admin socket, which prints the event to the terminal.
