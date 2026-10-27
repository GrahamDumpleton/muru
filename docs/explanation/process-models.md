# Process models, threads and the GIL

```{admonition} Status
:class: note

Everything on this page is implemented except handing a request across
between WSGI and ASGI after it has started, described under
[mixing WSGI and ASGI](#mixing-wsgi-and-asgi-in-one-application); mounts,
which choose the side by path, are implemented.
```

Your application runs in worker processes. This page explains how those
processes come to exist, what runs your code inside them, and how to choose
between processes and threads.

## Host and workers

Each release has a host process. The host starts a Python interpreter,
imports your application once, and then forks a worker for each process the
release is configured with. A worker is a copy of the host at the moment of
the fork: the imported modules, the loaded configuration and the warmed
caches are all already there, shared with the host and with the other
workers through copy-on-write memory until something writes to them.

The host never serves a request. Its only jobs are to fork workers and to
replace them when they exit, crash, or are recycled. Replacing a worker
costs a fork rather than a fresh import, which is what keeps restarts,
recycling and scaling fast.

Because the application is imported before the fork, anything the import
creates that cannot be shared across a fork, such as an open database
connection or a background thread, has to be created again in each worker.
Muru checks for this after the import and, by default, falls back to
importing in each worker with a warning naming what it found. The
`post_fork` hook is the place to reopen such things when you want to keep
the preload.

Before each fork the host freezes the garbage collector's view of the
preloaded objects so that collections in the workers do not touch, and so
copy, the shared pages. Workers then collect their own garbage normally.

## WSGI: a fixed pool of long-lived threads

A WSGI worker has a fixed number of application threads, created when the
worker starts and kept for its whole life. A request is handed to an idle
thread, runs on it, and the thread returns to the pool. The threads are real
Python threads with stable identities, so:

- **Thread-local storage survives across requests.** A database connection,
  a parser, or any cache kept in a `threading.local` is reused by every
  request that lands on that thread.

- **The number of threads is the number of requests a worker runs at
  once.** It is also the pool size the edge keeps for the worker, so
  requests beyond it queue at the edge rather than inside Python.

- **Blocking is fine.** A thread waiting on a database, a socket or the
  request body holds nothing but itself. While it waits for the request body
  or for a slow client to accept the response, the interpreter is released
  so other threads run.

Each request runs inside its own fresh context for Python's `contextvars`.
A value set during one request, such as a request id bound into a logger, is
gone when the next request runs on the same thread. Thread-locals are for
per-thread resources and persist. Context variables are for per-request
values and do not.

## ASGI: one event loop per worker

An ASGI worker runs a single event loop thread, started after the fork, on
which every connection's coroutine runs. The configured concurrency is how
many requests the worker accepts at once, and WebSockets have a separate
limit because they live longer.

Synchronous code called through `sync_to_async` or `asyncio.to_thread` runs
on a separate bounded thread pool in the same worker, so a blocking call
cannot stall the loop.

## Choosing processes and threads

| Model | Settings | Suits |
| --- | --- | --- |
| Threaded WSGI | several processes, several threads each | Most WSGI applications, which spend their time waiting on databases and services |
| Process per request | many processes, one thread each | Code that is not thread-safe, or CPU-bound views on a regular interpreter |
| Async ASGI | several processes | ASGI applications, many idle connections, WebSockets |
| ASGI with a sync pool | several processes, a sync thread pool | ASGI frameworks running synchronous views |
| Free-threaded | few processes, many threads | WSGI on a free-threaded interpreter, where threads run Python in parallel |

Processes isolate: a crash, a memory leak or a stuck request takes down one
worker and the host replaces it. Threads share: they are cheap, they share
caches and connection pools, and on a regular interpreter they take turns
holding the GIL, so adding threads helps a worker that is waiting and does
not help one that is computing. Muru reports, per worker group, whether
workers are busy on CPU, waiting for the GIL, waiting on I/O with every
thread occupied, or idle, so the choice can be made from measurements.

A release can split its paths across worker groups with different settings,
for example many threads for a cheap status endpoint and single-threaded
processes for a report generator, all forked from the same host.

## The GIL and free-threaded Python

On a regular interpreter only one thread runs Python at a time within a
process. Threads still pay off for WSGI because most of a request is spent
waiting, and the bridge releases the interpreter at every wait. Parallel
computation needs processes.

On a free-threaded interpreter (the `t` builds, 3.14t and later) threads run
Python in parallel, and a worker with many threads can use every core. The
runtime is built for these interpreters from the start: nothing in it relies
on the GIL for safety, and the same code runs on both kinds of build. A
release can require that the interpreter really is running free-threaded,
rather than quietly falling back because an imported extension module
switched the GIL back on.

## Mixing WSGI and ASGI in one application

A worker that serves both interfaces has application threads and an event
loop, and a request can move between them. Mounts decide by path, at the
edge, which side serves a request. When the decision belongs in code, a
request can be handed across after it has started, in either direction.
This is how a WSGI application takes WebSockets: the edge handles the
WebSocket protocol itself and only passes whole messages to the worker, so
a WSGI view hands the upgrade to an ASGI application and Muru provides a
small one for the common case.

**From WSGI to ASGI.** On a worker with an event loop, the environ carries
`muru.wsgi_to_asgi`. Its absence is the feature test. The object holds the
ASGI scope the request would be given, says whether the request is a
WebSocket upgrade, and has a `handoff` method that continues the request as
an ASGI application on the loop and returns at once. The view then returns,
and its thread goes back to the pool.

```python
from muru.asgi import websocket

class Chat:
    def on_connect(self, ws):
        rooms.join(ws, "lobby")

    def on_receive(self, ws, message):
        for peer in rooms.members("lobby"):
            peer.send(message)

    def on_disconnect(self, ws, code, reason):
        rooms.leave(ws)


def application(environ, start_response):
    bridge = environ.get("muru.wsgi_to_asgi")
    if bridge is None or not bridge.is_websocket:
        start_response("426 Upgrade Required", [("Content-Type", "text/plain")])
        return [b"websocket required\n"]

    bridge.scope["state"]["user"] = current_user(environ)
    return bridge.handoff(websocket.Handler(Chat(), subprotocol="chat"))
```

Declining is ordinary WSGI: respond with any status and the client receives
it as the answer to its upgrade. Values the view puts in the scope's
`state`, such as the authenticated user, reach the ASGI side the way ASGI
middleware passes them, and the request's context variables come across
too.

This works from inside a framework. After a handoff, Muru ignores the
status, headers and body the framework goes on to produce for the request,
closes the response iterable without consuming it, and continues the
request on the loop. `handoff()` returns an empty iterable that is also a
WSGI callable, so a plain WSGI application or a Flask view returns it
directly; a Django view hands off and returns an empty `HttpResponse`.
Nothing in the framework needs to know.

The handler above is Muru's WebSocket adapter: an ordinary ASGI application
that accepts the socket and calls `on_connect`, `on_receive` and
`on_disconnect` on application threads, one call at a time per socket and
in order, with every call for a socket inside the same context so a value
set in `on_connect` is visible later whichever thread runs it. Idle
sockets cost no Python thread. `send` can be called from any thread, which
is how a broadcast is written, and a handler must not block waiting for
another message on its own socket. The adapter runs under any ASGI server,
and an application that already has a Django Channels consumer or a
Starlette endpoint hands that off instead and never uses it.

Anything else that is an ASGI application can be handed off the same way:
an async view, a Starlette sub-application, a streaming endpoint. An
application that needs ASGI's lifespan startup and shutdown is registered
once from a `post_fork` hook with `muru.asgi.register`, which runs its
startup in each worker before the worker is ready and its shutdown when
the worker drains.

**From ASGI to WSGI.** The mirror image: an ASGI application finds
`asgi_to_wsgi` among the Muru entries in its scope's extensions, on HTTP
requests, and hands the request to a WSGI callable, which then runs on an
application thread with a normal environ. Nothing is buffered on the way,
the rest of the request body streams into wsgi.input as it arrives, and the
scope's state is available as `environ["muru.state"]`. If every thread is
busy the request waits for one, bounded by its deadline, and Muru stops
sending new thread-bound requests to that worker until a thread frees.

Both directions cost a little more than a request that goes straight to
the right side, so when the routing is static a mount is the better tool.
The handoffs are for decisions that only the code can make.

For the detail, see the design specification's
[Python runtime](../project/design/06-python-runtime.md) and
[process management](../project/design/07-process-management.md) pages.
