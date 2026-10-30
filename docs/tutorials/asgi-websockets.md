# Run an ASGI app with WebSockets

```{admonition} Status
:class: note

This tutorial covers development mode, like
[the first one](first-wsgi-app.md), which it assumes you have done.
```

In about ten minutes you will serve a small chat room as an ASGI
application with `muructl run`, talk to it over WebSockets from two
terminals, watch the sockets in the access log and the dashboard, and see
what happens to open sockets when the server stops or reloads, and when
there is more than one worker.

## A project

Make a project directory with the runtime library in it, as in the first
tutorial:

```
mkdir chat && cd chat
uv init --bare
uv add muru
```

## A chat room

Save this as `chat.py`:

```{literalinclude} code/chat/chat.py
:language: python
```

An ASGI application is one coroutine that Muru calls with a scope, which
says what kind of connection it is, and two functions for messages in
and out. This one handles all three kinds of scope it will see:

- `lifespan` happens once in each worker, before it serves anything and
  again when it stops. The application makes an empty set of chat
  members here and keeps it in `scope["state"]`.

- `http` is an ordinary request. Muru copies the lifespan state into
  every request's scope, so the page can count the members.

- `websocket` is a WebSocket. The application accepts it, adds the
  socket to the members, and passes each message it receives on to
  every other member until the client goes away.

## Run it

```
muructl run chat:application
```

The terminal shows:

```
muru.host[4242]: preloaded chat:application in 1 ms
muru.host[4242]: chat:application is served as ASGI
muru.host[4242]: worker 0 started as pid 4243
muru.host[4242]: worker 0 (pid 4243) is ready
muru: serving chat:application on http://127.0.0.1:8080 with 1 worker of concurrency 1000
muru: development mode: error pages show tracebacks, nothing is persisted and only loopback is served; production runs under the daemon (muructl daemon start)
muru: dashboard on http://127.0.0.1:8081/
```

Muru saw that `application` is a coroutine function and serves it as
ASGI. Where a WSGI worker has threads, an ASGI worker has one event loop,
and every request and every open WebSocket is a task on it, up to the
concurrency of 1000 at once. Ask the page how many are in the chat:

```
curl http://127.0.0.1:8080/
```

```
0 in the chat
```

## Join the chat

Open two more terminals and join from each with the command-line client
that comes with the `websockets` package:

```
uvx websockets ws://127.0.0.1:8080/chat
```

Each says `Connected to ws://127.0.0.1:8080/chat.` and gives you a `>`
prompt. Type a line in one and it appears in the other, marked with `<`:

```
< hello from the first terminal
```

`curl http://127.0.0.1:8080/` now says `2 in the chat`. In the server's
terminal, each join added an access log line:

```
09:50:21.230 101 GET /chat 0B 0.5ms queue=0.0ms app=0.5ms worker=0 id=01M60YNX1EJ9HET9KK5Z2HR53G
```

The 101 is the HTTP status that switches a connection to the WebSocket
protocol. The line is written when the application accepts the socket,
and the socket then stays open as long as both ends want it.

## Leave the chat

Press Ctrl-D in one of the clients. It closes its socket, the
application gets `websocket.disconnect` and takes the member out of the
set, and the access log gets a second line for the same request id,
saying who closed the socket, with what code, and how many messages went
each way:

```
09:50:30.512 101 GET /chat 14B 9.28s websocket=closed by=client code=1000 received=1 sent=1 worker=0 id=01M60YNX1EJ9HET9KK5Z2HR53G
```

## Watch the sockets

Open http://127.0.0.1:8081/ while a client is connected. Beside the
requests, the dashboard shows the WebSockets open, by path, with how long
each has been open and what has passed each way, and charts them over
time. The same list is JSON at http://127.0.0.1:8081/v1/websockets.

## Stop with sockets open

Join from both terminals again, send a line from each, and press Ctrl-C
in the server's terminal. Both clients say:

```
Connection closed: 1001 (going away).
```

A worker that stops takes no new requests, closes its open WebSockets
with 1001, which tells a client the server is going away and that it can
connect again, and then exits. The access log lines say
`websocket=closed by=edge code=1001`.

The same happens on a reload. Start the server with `--reload`, join
from a client, and change the text of the page in `chat.py`. The terminal
says `switched to release r2`, the client's socket is closed with 1001,
and a client that joins again reaches the new code. The chat starts
empty, because the new worker ran its own lifespan and made its own set.

## More than one worker

Stop the server and start it with two workers:

```
muructl run chat:application --processes 2
```

Join from both terminals and send a line. Most likely the other terminal
sees nothing, and the access log shows why: one socket went to
`worker=0` and the other to `worker=1`. Each worker is a separate process
with its own event loop and its own lifespan, so each has its own set of
members, and `curl` reports one or the other.

A real chat service would pass messages between workers through
something they share, such as Redis publish and subscribe. One worker of
concurrency 1000 is plenty for a laptop, and the
[process models](../explanation/process-models.md) page explains when more
workers help.

## From here

- [ASGI applications](../howto/asgi.md) covers the event loop, the
  thread pool for synchronous code, lifespan, request bodies and every
  detail of WebSocket closes.

- [Starlette and FastAPI](../howto/starlette-and-fastapi.md) run
  unchanged, WebSocket routes included.

- [Deploy with the daemon and roll back](deploy-and-roll-back.md) takes an
  application to the daemon, where `muructl websockets` counts the open
  sockets of every application.
