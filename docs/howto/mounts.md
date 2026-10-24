# Mounts

```{admonition} Status
:class: note

Named mounts at path prefixes, any mix of WSGI and ASGI in the same
workers, in development mode and under the daemon. Per-mount thread
pools and connection limits, internal mounts, gRPC mounts and the groups
`mounts` subset are still to come.
```

A release can serve several entrypoints from the same worker processes,
and from one preload: the release's own entrypoint is the default mount
at `/`, and named mounts take a path prefix each. Mounts can be any mix
of WSGI and ASGI, which is the static way to run both interfaces in one
application.

Mounts ship with the application in its `[tool.muru]` table:

```toml
entrypoint = "chat.wsgi:application"
threads = 16

[mounts.ws]
path = "/ws"
entrypoint = "chat.asgi:websocket_app"

[mounts.reports]
path = "/reports"
entrypoint = "chat.reports:application"
interface = "wsgi"
```

or in an applied file, under the release, where they replace the table's:

```toml
[app.chat.release.r7.mounts.ws]
path = "/ws"
entrypoint = "chat.asgi:websocket_app"
```

A mount has a name (lowercase letters, digits and hyphens), a `path`
below `/`, an `entrypoint` as `module:callable`, and an optional
`interface` where detection from the callable is not wanted. No two
mounts share a path. The longest mount path wins, matched on segment
boundaries like routes: `/reports` covers `/reports` and `/reports/daily`
and not `/reportsx`, and everything unclaimed goes to the default mount.

## What the application sees

The mount's path is `SCRIPT_NAME` and the rest of the path is `PATH_INFO`
(`""` for a request to the mount's path itself); behind a trusted proxy
that forwarded a prefix, `SCRIPT_NAME` is the prefix and the mount's path
joined. For ASGI, `root_path` is the mount's path and `path` is the whole
path, as the ASGI specification asks. `muru.context().interface` names
the default mount's interface.

A worker with any ASGI mount runs one event loop, and every ASGI
application on it, the default mount's or a named one's, has its own
lifespan: each is started before the worker is ready and shut down when
it stops, with its own `state` copied into its requests. WSGI mounts share
the worker's thread pool. A worker whose default mount is WSGI is sized by
its threads at the edge, one whose default is ASGI by its concurrency;
per-mount pools are still to come.

Mounts share one interpreter: two applications that rely on process-wide
globals need separate releases.

## Under the daemon

`muructl deploy` takes the mounts from the source's table and `muructl
apply` from the release's `mounts` tables when present. Mounts are part
of the release, so a declared release whose mounts differ from what is
recorded is refused, and `muructl config export` writes them with the
release.
