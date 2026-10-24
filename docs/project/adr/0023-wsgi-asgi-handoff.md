---
status: accepted
date: 2026-10-24
---

# ADR 0023: Hand requests between WSGI and ASGI inside a worker, with WebSockets for WSGI built on it

## Context and problem statement

The specification previously said WSGI paths answer WebSocket upgrades with
426 and only ASGI mounts take them. With the framed link of ADR 0021, the
edge owns the WebSocket wire protocol and the worker sees only whole
messages, so a WSGI application could take a socket cheaply. The first
design was a WebSocket-only environ extension with two forms, one holding
the thread and one delegating to a callback handler. Working it through
showed that the handler form was a special case of something more general:
once a WSGI function has returned, the bridge can continue the request as an
ASGI application on the worker's event loop, for WebSockets and HTTP alike,
and the reverse hand-over from ASGI to a WSGI callable on the thread pool
is its mirror. Specification section 6.14 defines the result.

## Decision drivers

- Many applications are WSGI and will stay WSGI. Adding a WebSocket
  endpoint, or an async view, should not force a move to ASGI and a second
  concurrency model, and an ASGI application should be able to keep a WSGI
  part without mounts.

- Frameworks hide start_response and the return value, so the mechanism
  must work from inside a Flask or Django view without framework support.

- One mechanism, not a family of disjoint extensions, and one set of rules
  for what happens to the WSGI response after a hand-over.

- Prior art shows the demand and the pitfalls: gevent-websocket's
  wsgi.websocket did the handshake before the application could decline;
  uWSGI's websocket calls hold the worker core; simple-websocket and
  Flask-Sock hijack the raw socket through a different private key on every
  server; asgiref and a2wsgi run WSGI under ASGI by buffering the body and
  using their own threads.

- A thread per open socket is acceptable for a handful and not for
  thousands; idle sockets should cost no Python thread.

## Considered options

- No extension: WSGI gets 426, WebSockets and async code need ASGI mounts.

- A WebSocket-only extension in environ, with a thread-holding form and a
  handler form.

- A general hand-over in both directions: an environ object that continues
  the request as an ASGI application, a scope extension that continues it
  as a WSGI callable, lifespan registration for handed-off applications, and
  a WebSocket callback adapter that is itself an ordinary ASGI application.

## Decision outcome

Chosen option: the general hand-over. Environ carries `muru.wsgi_to_asgi`
with `scope`, `is_websocket`, `handoff(app)` and `handed_off`; the scope
carries `extensions["muru"]["asgi_to_wsgi"]` with `handoff(app)` on http
scopes. After a hand-over the bridge ignores the WSGI response and closes
the iterable without consuming it, or requires the coroutine to return,
carries the request's context across, and streams whatever body remains.
The worker tells the edge with a DETACH frame so the connection is counted
against the right bound; in the ASGI to WSGI direction a request waits in a
bounded in-worker queue for a thread, which the edge compensates for by
dispatching nothing new to that worker's threads until one frees. Lifespan
for handed-off applications is `muru.asgi.register(app)` from post_fork,
never lazy. WebSockets for WSGI are `muru.asgi.websocket.Handler`, an
ordinary ASGI application with Starlette's method names, dispatching
callbacks to the thread pool one at a time per socket in one context per
socket.

The thread-holding WebSocket form was considered and not offered: the
adapter gives the same programming model without occupying a thread, and
porting code written against a blocking socket is rare enough to be an
adapter added later if wanted. Socket hijack was rejected because the link
carries frames, not a client socket, and because it makes every application
carry a WebSocket implementation. Generic sync_to_async and async_to_sync
helpers were left to asgiref (section 20.9).

### Consequences

- Good: WebSockets and async views from existing WSGI views, and WSGI
  callables from ASGI applications, with the edge doing the protocol work
  and the link unchanged; Channels consumers and Starlette endpoints work
  through the same call; idle sockets cost no Python thread; one set of
  rules covers both directions and middleware.

- Good: the adapter is portable Python that runs under any ASGI server,
  and the only Muru-specific pieces are the ones only a server can provide.

- Bad: an application that hands off and then returns a body is not an
  error, so a bug of that shape is silent apart from a debug log line.

- Bad: the ASGI to WSGI direction introduces the one bounded queue inside a
  worker, and a request that crosses pays for a coroutine and a thread;
  static routing should use mounts, and the docs say so.

- Bad: no coroutine form in the WSGI worker beyond the handoff itself;
  applications wanting coroutines write an ASGI application and hand to it.

## More information

Specification sections 5.1, 5.2 (DETACH, async_slots), 5.6, 6.6, 6.7, 6.9,
6.14, 7.3, 11.1, Appendix D and 20.9. Implementation is scheduled with the
ASGI bridge and WebSockets in phase 2.
