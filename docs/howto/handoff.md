# Handing requests between WSGI and ASGI

```{admonition} Status
:class: note

Handing a WSGI request, or a WebSocket upgrade, to an ASGI application
with `handoff = "asgi"`.
```

[Mounts](mounts.md) choose between a WSGI and an ASGI application by
path. When the choice belongs in code, a request that has reached one
can be handed to the other in the same worker. A WSGI view takes a
WebSocket this way: it hands the upgrade to an ASGI application, which
accepts the socket on the worker's event loop, and the view's thread
goes back to the pool.

## Enabling it

Handing requests off is enabled in the configuration, for the release's
own entrypoint or for a mount, by naming the interface the requests may
be handed to:

```toml
entrypoint = "chat.wsgi:application"
handoff = "asgi"

[mounts.admin]
path = "/admin"
entrypoint = "chat.admin:application"
handoff = "asgi"
```

A WSGI application hands requests to `"asgi"`. With the setting, the
worker runs an event loop for the applications its requests are handed
to, and WebSocket upgrades to that application's paths reach it as
WebSockets. Without it, an upgrade is an ordinary request.

## From WSGI to ASGI

Every WSGI environ carries `muru.wsgi_to_asgi`. Its `handoff(app)`
continues the request as `app`, an ASGI application, and returns at once
with an empty response for the WSGI callable to return:

```python
async def chat(scope, receive, send):
    await receive()  # websocket.connect
    await send({"type": "websocket.accept"})
    user = scope["state"]["user"]
    ...


def application(environ, start_response):
    bridge = environ["muru.wsgi_to_asgi"]

    if not bridge.is_websocket:
        start_response("426 Upgrade Required", [("Content-Type", "text/plain")])
        return [b"websocket required\n"]

    bridge.scope["state"]["user"] = current_user(environ)
    return bridge.handoff(chat)
```

The object has:

| Attribute | Meaning |
| --- | --- |
| `scope` | The ASGI scope the application will receive, built from the request. Values put in `scope["state"]` before the handoff reach the application |
| `is_websocket` | Whether the request is a WebSocket upgrade |
| `handoff(app)` | Continue the request as `app` on the event loop. Raises `RuntimeError` once the response has started, which only a call to `write()` causes, or when the request has already been handed off |
| `handed_off` | Whether the request has been handed off, for middleware |

To decline a WebSocket, respond as usual: the client gets the status and
body as the answer to its upgrade.

An HTTP request can be handed off too, after the WSGI code has read some
of its body: the application receives the rest, as it arrives. The
request's context variables are copied to the application's task.

## Inside a framework

After a handoff, Muru ignores the status, headers and body the WSGI side
goes on to produce, and closes its response iterable without reading
it. A framework view can therefore hand off and return any response. The
object `handoff()` returns is also a WSGI application, so a Flask view
returns it directly:

```python
@app.route("/chat")
def chat_view():
    bridge = request.environ["muru.wsgi_to_asgi"]
    return bridge.handoff(chat)
```

A Django view hands off and returns an empty response:

```python
def chat_view(request):
    request.environ["muru.wsgi_to_asgi"].handoff(chat)
    return HttpResponse()
```

## When it is not enabled

Without `handoff = "asgi"`, the environ key is still there and
`handoff()` raises `muru.HandoffNotEnabled`, a `RuntimeError`, whose
message names the setting to add:

```
muru.HandoffNotEnabled: mount admin does not enable handing requests to ASGI;
set handoff = "asgi" on the mount
```

A worker does not start when a handoff names the interface its
application already speaks, such as `handoff = "asgi"` on an ASGI
application, or when a gRPC mount has one.

Outside Muru, under another server, the key is absent, so code that
also runs elsewhere checks for it with `environ.get("muru.wsgi_to_asgi")`.
