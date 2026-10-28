# Worker groups

```{admonition} Status
:class: note

Named groups with their own process and thread counts, selected by path
prefix, in development mode and under the daemon, each with its own
scaling policy (see [scaling](scaling.md)), timeouts and memory policy,
and optional groups the release is ready without. Match lists with host
and method conditions and the `mounts` subset are still to come.
```

A release can split its paths across named worker groups, all forked from
one host and one preload. Each group has its own processes and threads,
which gives sizing (many threads for cheap paths, several processes for
heavy ones) and isolation (a slow endpoint cannot take the capacity of a
fast one). Without groups there is one group, `default`, which is the
release's own settings.

Groups ship with the application in its `[tool.muru]` table, in
`pyproject.toml` or `muru.toml`:

```toml
entrypoint = "shop.wsgi:application"
processes = 2
threads = 8                       # the default group

[groups.hot]
paths = ["/api/status", "/api/ping"]
processes = 1
threads = 50

[groups.reports]
paths = ["/reports/*"]
processes = 6
threads = 1
```

or in an applied file, under the release, where they replace the table's:

```toml
[app.shop.release.r42.groups.reports]
paths = ["/reports/*"]
processes = 6
threads = 1
```

`paths` are prefixes, matched on segment boundaries like routes (`/reports`
covers `/reports` and `/reports/daily`, not `/reportsx`); a trailing `*`
means the same thing and is accepted for readability. The most specific
match across all groups wins, and everything unclaimed goes to the default
group. A group's `processes`, `threads`, `sync_threads` and `concurrency`
default to the release's own values. A group needs at least one path, or
a `match`, and one process, cannot be named `default`, and takes a name of
lowercase letters, digits and hyphens.

Under the daemon, `match = { client_class = [...] }` makes a group take
the requests of those [client classes](client-classes.md#routing-by-class)
for its paths, or for every path when it has none, ahead of the groups
for every class.

`muructl run` prints a line for each group when it starts, after the
serving line, which describes the default group:

```
muru: serving shop.wsgi:application on http://127.0.0.1:8080 with 2 workers of 8 threads
muru: group hot serves /api/status, /api/ping with 1 worker of 50 threads
muru: group reports serves /reports/* with 6 workers of 1 thread
```

## Timeouts and memory per group

A group can have [timeouts](timeouts.md) and a [memory
policy](memory.md) of its own, so a group of slow report paths can be
given a long hard timeout and a large memory limit while the rest of the
application keeps short ones:

```toml
[groups.reports]
paths = ["/reports/*"]
processes = 2
threads = 1
timeouts = { soft_timeout = "120s", hard_timeout = "300s" }
memory = { hard_limit = "2GB" }
```

Each key a group leaves out is the release's: its `timeouts` and
`memory` tables, or the `--timeout` and `--memory` settings given on the
command line. `shutdown_timeout` and `websocket_drain_grace` belong to
the release, since they bound how long every worker gets to stop, and a
group may not set them. A group's hard timeout must exceed its soft
timeout by at least five seconds, as the release's must. `muructl config
export` writes a group's memory policy with every key spelled out and
its timeouts as it set them.

## Optional groups

A release is normally ready only when every worker of every group
listens. A group marked `optional = true` is one the release is ready
without:

```toml
[groups.batch]
paths = ["/batch/*"]
optional = true
processes = 1
```

The release is ready once the default group and every required group
listen, and the optional group's workers join as each one does. Until the
group has a worker, a request for its paths waits at the edge, as it
does for a group scaled to zero, and is answered 503 at the queue
timeout; it is not sent to the default group, since the paths were given
to the group for its own sizing or isolation, and serving them from
elsewhere would hide that the group is down.

A worker of a required group that fails three times at start stops the
release. A worker of an optional group that does so is given up instead,
with a `worker.exited` event that names the group, and the release keeps
serving on the others; `muructl scale` with a count for the group forks
it again. `muructl run` and `muructl release show` mark such a group
`optional`, as does the dashboard's scaling table, and the `release.ready`
event names an optional group whose workers were not all listening when
the release became ready.

## In the workers

Each worker knows its group: `muru.context().group` is the group's name,
`muru.context().worker_id` the worker's index within the group, and the
environment variable `MURU_GROUP` is set before the application is
imported in the worker, so a post-fork hook can size caches or pools for
the group it is in. Groups share the interpreter and everything imported
before the fork; differences must be applied after it.

## Under the daemon

`muructl deploy` takes the groups from the source's table; `muructl apply`
takes them from the release's `groups` tables when present. A release is
ready when every worker of every required group listens, and `muructl
events` counts the workers listening. Groups are part of the release, so a declared release
whose groups differ from what is recorded is refused like any other
changed setting; `muructl config export` writes each group with every size
spelled out, and a restarted daemon takes every group's workers back.
`muructl release show` lists a release's groups with their paths and
sizes.
