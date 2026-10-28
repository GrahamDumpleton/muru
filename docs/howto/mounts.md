# Mounts

```{admonition} Status
:class: note

Named mounts at path prefixes, any mix of WSGI and ASGI in the same
workers, in development mode and under the daemon, internal mounts,
threads of a WSGI mount's own and the concurrency and connection limits
of an ASGI mount's own. [gRPC mounts](grpc.md) have a page of their own.
The groups `mounts` subset is still to come.
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

A mount may also carry the [internal redirect](internal-redirects.md)
settings: `internal = true` keeps clients out of it, so only a redirect
from the release's own application, or from one its
`accept_redirects_from` names, reaches it; `accel_redirect` and
`cgi_local_redirect` say which redirect forms its responses may use,
with the release's own setting as the default.

When `muructl run` starts, the host says what each entrypoint is served
as, and a line for each mount says where it is. A WSGI release with an
ASGI mount has an event loop in every worker beside the threads, and the
serving line says so:

```
muru.host[4242]: chat.wsgi:application is served as WSGI
muru.host[4242]: mount ws: chat.asgi:websocket_app is served as ASGI
muru.host[4242]: mount reports: chat.reports:application is served as WSGI
muru: serving chat.wsgi:application on http://127.0.0.1:8080 with 1 worker of 16 threads and an event loop of concurrency 1000
muru: mount reports serves /reports from chat.reports:application
muru: mount ws serves /ws from chat.asgi:websocket_app
```

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
the worker's thread pool unless they have threads of their own. A worker
whose default mount is WSGI is sized by its threads at the edge, one whose
default is ASGI by its concurrency.

## Threads of a mount's own

A WSGI mount's `threads` gives it threads that only it uses, so the rest
of the application cannot take them all. A number is a pool of its own:

```toml
[mounts.reports]
path = "/reports"
entrypoint = "chat.reports:application"
threads = 4
```

and a table reserves some threads and lets the mount borrow the worker's
shared threads beyond them, up to a maximum of requests at once:

```toml
[mounts.reports]
path = "/reports"
entrypoint = "chat.reports:application"
threads = { reserved = 2, max = 6 }
```

Here two of the mount's requests always have a thread, and up to four
more run on the shared threads when those are free. `reserved = 0` caps
the mount's share of the shared threads without reserving any. Requests
beyond what a mount can run wait at the edge, as they do for the shared
threads, until the queue timeout. `wsgi.multithread` is True when the
mount can run more than one request at once.

On a [gRPC mount](grpc.md), `threads` is a number: the threads its
synchronous handlers run on. An ASGI mount runs on the event loop and
takes no `threads`.

## Limits of an ASGI mount's own

An ASGI mount runs on the worker's event loop and, without limits of its
own, shares the worker's slots: beside a WSGI application its requests
and open WebSockets take the WSGI threads' places at the edge, so a few
hundred open sockets can hold every thread. Two settings give the mount
a place of its own on each worker:

```toml
[mounts.ws]
path = "/ws"
entrypoint = "chat.asgi:websocket_app"
interface = "asgi"
concurrency = 500
max_connections = 2000
```

`concurrency` is the requests the mount runs at once, slots the edge
holds for it apart from the worker's others, and `max_connections` the
open WebSockets it holds at once. A WebSocket handshake takes one of
each; once the application accepts the socket it gives its request slot
back and keeps its place among the connections for the life of the
socket, so the mount's requests are not held up by its sockets. Requests
and handshakes beyond either limit wait at the edge until the queue
timeout. Either may be set alone: with `concurrency` alone, open
sockets hold request slots as they do without limits, and with
`max_connections` alone, requests share the worker's slots. Both need
the mount declared `interface = "asgi"`, and both are per worker, like
`threads`.

Mounts share one interpreter: two applications that rely on process-wide
globals need separate releases.

## Under the daemon

`muructl deploy` takes the mounts from the source's table and `muructl
apply` from the release's `mounts` tables when present. Mounts are part
of the release, so a declared release whose mounts differ from what is
recorded is refused, and `muructl config export` writes them with the
release. `muructl release show` lists a release's mounts with their
paths, entrypoints and interfaces, as declared or as detected.
