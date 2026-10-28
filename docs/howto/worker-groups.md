# Worker groups

```{admonition} Status
:class: note

Named groups with their own process and thread counts, selected by path
prefix, in development mode and under the daemon, each with its own
scaling policy (see [scaling](scaling.md)), timeouts and memory policy,
optional groups the release is ready without, match lists that ask a
client class, host or method of a request beside its path, and a
`mounts` subset a group's workers serve.
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

## Match lists

A group's `match` table asks more of a request than its path: the
[client classes](client-classes.md#routing-by-class) it takes, the hosts
it serves, or the methods it handles. A request matches the group when
its path is under one of the group's paths, or any path for a group
with none, and every list the group gives names the request's class,
host or method:

```toml
[groups.uploads]
paths = ["/files/*"]
match = { method = ["POST", "PUT"] }
processes = 2

[groups.partner]
paths = ["/reports/*"]
match = { host = ["partner.example.com"] }
threads = 4

[groups.crawlers]
match = { client_class = ["ai_crawler"] }
processes = 1
```

`host` names a [virtual host](virtual-hosts.md) by its configured name or
by one of the names it answers to, written exactly, and under `muructl
run` the name the request carries; `method` is compared in upper case;
`client_class` names classes the daemon defines. A header condition is
written once as a client class and the group names the class.

A group asking more of a request comes before one asking less: a group
with two lists is tried before a group with one, and a group with a list
before a group with none, so a class's group still comes ahead of the
groups for every class. Among groups asking as much, the longer path
prefix wins. Two groups that ask the same of the same path are refused,
since nothing could order them; two that ask different methods of the
same path are the usual way to split reads from writes.

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

## Mounts a group serves

Every worker serves every [mount](mounts.md) of the release. A group can
list the named mounts its workers serve instead, so the workers of a
group for one mount's paths do not start the lifespans or hold the
slots of the others:

```toml
[groups.reports]
paths = ["/reports/*"]
mounts = ["reports"]
processes = 2
```

The default mount, which is the release's entrypoint and carries its
hooks, is always served. The host still imports every mount before the
fork, so a broken entrypoint fails the release early as before and the
preload stays shared; a worker of the group realises, starts the
lifespan of, and declares to the edge only its own mounts. With
per-worker imports, a worker imports only its own.

A group's paths must lie within the mounts it serves: the release is
refused when a mount the group leaves out has its path under one of the
group's paths, or a group's path under the mount's, since a request for
that path could reach a worker without its mount. A group that claims
every path therefore serves every mount. A worker asked for a mount it
does not serve, which that check rules out, answers 503
`mount_not_loaded`.

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
