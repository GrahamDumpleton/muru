# ASGI applications

```{admonition} Status
:class: note

Muru serves ASGI over HTTP and WebSockets, in development mode and
under the daemon, with the `pathsend` and `zerocopysend` file extensions ([file hints](file-hints.md)).
Response trailers are not served yet. WebSockets over HTTP/2 (extended
CONNECT) are accepted but not yet covered by the test suite.
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
muru: serving app:application on http://127.0.0.1:8080 with 1 worker of concurrency 1000
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

## WebSockets

A WebSocket upgrade reaches the application as a `websocket` scope, with
`websocket.connect` as the first message from `receive()`. Send
`websocket.accept`, with a `subprotocol` and `headers` if you like, and
the edge completes the handshake; from then on each `websocket.receive`
carries one whole message as `text` or `bytes`, and each `websocket.send`
goes out as one. The edge owns the wire protocol: pings, pongs, masking
and fragments never reach the application.

Closing works in both directions. `websocket.close` with a `code` and
`reason` closes the client's socket with them; a client that closes, or
goes away, shows up as `websocket.disconnect` with its `code` (1006 for a
client that vanished without closing). Returning from the application
closes an open socket with 1000, and an exception closes it with 1011.
Sending `websocket.close` before accepting declines the handshake with
403, as does returning without accepting; to decline with a response of
your own, send `websocket.http.response.start` and
`websocket.http.response.body` instead.

A message larger than 1 MiB closes the socket with 1009. The edge pings a
client that has been quiet for 30 seconds and closes a socket that stays
silent for five minutes with 1001. Open sockets count against the worker's
`--concurrency`, not against any thread count.

## Watching WebSockets

A WebSocket's handshake is logged and counted as a request when the
application accepts it, and the open socket is then counted on its own
until it closes, when the [access log](logging.md#the-access-log) gets a
second line saying who closed it, with what code, and what passed each
way. The [metrics](metrics.md) have the open sockets as
`muru_websocket_connections`, the closes by outcome and how long sockets
stayed open, and the [dashboard](dashboard.md) shows them beside the
requests.

Under the daemon, `muructl websockets` counts the open sockets by
endpoint. Path segments that look like identifiers (numbers, UUIDs,
ULIDs, long hex or long tokens of letters and digits) are counted as
`{id}`, and a segment with more than twenty distinct values under the
same parent as `*`, so a socket per chat room is one row:

```console
$ muructl websockets
APP              ENDPOINT                                   OPEN
chat             /ws/rooms/{id}                              480
chat             /ws/notify                                   20
$ muructl websockets chat --list --limit 2
OPENED               FOR       RELEASE          PATH                             CLIENT           RECEIVED     SENT WORKER REQUEST ID
2026-10-27T05:42:23Z 12m 4s    chat@r3          /ws/rooms/1844                   203.0.113.9            12       40 0      01M5P6WZD3740HFT2N80MAV4YV
2026-10-27T05:42:31Z 11m 56s   chat@r3          /ws/rooms/2013                   198.51.100.4            3        9 1      01M5P6WZE1740HFT2N80MAV4YV
(2 of 500 open shown; --limit shows more)
```

`muructl status` and `muructl release show` give how many are open, which
is what keeps a [draining](daemon.md#releases) release's workers busy.

## Frameworks

Starlette, FastAPI and Django's ASGI handler run unchanged; the
[Starlette and FastAPI](starlette-and-fastapi.md) page has the details.

## What the scope contains

The [ASGI scope reference](../reference/asgi-scope.md) lists every key,
including Muru's own under `scope["extensions"]["muru"]`.
