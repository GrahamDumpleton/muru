# Worker groups

```{admonition} Status
:class: note

Named groups with their own process and thread counts, selected by path
prefix, in development mode and under the daemon. Scaling modes, per-group
timeouts and memory policy, match lists with host and method conditions,
optional groups and the `mounts` subset are still to come.
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
default to the release's own values. A group needs at least one path and
one process, cannot be named `default`, and takes a name of lowercase
letters, digits and hyphens.

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
ready when every worker of every group listens, and `muructl events`
counts them all. Groups are part of the release, so a declared release
whose groups differ from what is recorded is refused like any other
changed setting; `muructl config export` writes each group with every size
spelled out, and a restarted daemon takes every group's workers back.
