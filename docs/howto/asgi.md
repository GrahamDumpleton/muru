# ASGI applications

```{admonition} Status
:class: note

Development mode serves ASGI over HTTP. WebSockets, response trailers and
the `pathsend` and `zerocopysend` file extensions are not served yet; a
WebSocket upgrade reaches the application as an ordinary `http` scope.
```

Point `muructl run` at the ASGI callable, exactly as for WSGI:

```
muructl run app:application
```

Muru detects an ASGI application from its callable: a coroutine function,
or an object whose `__call__` is one. When detection cannot see it, for
example a class whose synchronous `__call__` returns a coroutine, declare
it with `--interface asgi`, or with `interface = "asgi"` in the project's
`[tool.muru]` table. A callable that is a coroutine function but is
declared `wsgi` is refused at start-up.

The start-up output says which interface the application is served with,
and the serving line reports the loop's concurrency instead of a thread
count:

```
muru.host[4242]: app:application is served as ASGI
muru: serving app:application on http://127.0.0.1:8080 with 1 workers of concurrency 1000
```

## One event loop per worker

Each worker runs one asyncio event loop on a thread of its own, started
after the fork. Every request runs as a task on that loop, inside its own
`contextvars` context, so a context variable set during one request is
not seen by another. Up to `--concurrency` requests (default 1000) are in
flight on the loop at once; the edge queues beyond that, as it does for
threads, and answers 503 when the queue timeout passes.

Synchronous code the application runs through `asyncio.to_thread`, or
through `sync_to_async` from asgiref, goes to a thread pool of
`--sync-threads` threads (default 8) that belongs to the loop. Both
settings can also be given as `concurrency` and `sync_threads` in
`[tool.muru]`.

## Lifespan

The lifespan protocol runs once in each worker. Startup runs before the
worker accepts requests, after `post_fork` and the factory if there is
one, and before `on_ready`; an application that answers
`lifespan.startup.failed`, or raises after it has started handling the
lifespan, fails the worker, which is reported like any other start-up
failure. An application that raises before answering `lifespan.startup`
does not support lifespan, which is allowed, and is served without it.

Whatever the application puts in `scope["state"]` during startup is copied
into the `state` of every request's scope. Shutdown is sent when the
worker stops, after the requests in flight have finished and before the
`on_exit` hook.

## Request bodies and disconnects

The request body arrives through `receive()` as `http.request` messages,
the last with `more_body` false. A body sent without `Content-Length` is
buffered by the edge first, so the headers in the scope carry a
`content-length` for it. Once the response is complete, or the client has
gone away, `receive()` returns `http.disconnect`, and keeps returning it.

A response starts with `http.response.start` and ends with the
`http.response.body` whose `more_body` is false. Sending them out of
order, sending a second start, or sending anything after the response has
completed raises `RuntimeError` in the application. An exception before
the response has started is answered with the 500 error page, which
carries the traceback in development mode; one after output has reached
the edge ends the response short. A declared `Content-Length` is enforced:
output beyond it is dropped, and a body that ends short of it is reported
in the log. Returning from the application without completing a response
is a 500 too.

## Frameworks

Starlette, FastAPI and Django's ASGI handler run unchanged:

```
muructl run myproject.asgi:application
muructl run main:app
```

Django's synchronous views run on the sync thread pool, so give it as many
`--sync-threads` as you would give threads to a WSGI deployment.

## What the scope contains

The [ASGI scope reference](../reference/asgi-scope.md) lists every key,
including Muru's own under `scope["extensions"]["muru"]`.
