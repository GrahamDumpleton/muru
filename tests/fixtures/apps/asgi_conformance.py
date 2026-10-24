"""The application behind the ASGI conformance suite (tests/conformance/asgi).

Each path exercises one rule of ASGI 3.0 as the suite expects it; what the
application saw is reported back as JSON or printed to stdout, where the
suite reads the worker's log.
"""

from __future__ import annotations

import json
import os
from collections.abc import Awaitable, Callable, MutableMapping
from typing import Any

Scope = MutableMapping[str, Any]
Receive = Callable[[], Awaitable[dict[str, Any]]]
Send = Callable[[dict[str, Any]], Awaitable[None]]


def jsonable(value: Any) -> Any:
    """Make a scope value JSON-friendly: bytes as latin-1 text, pairs as lists."""

    if isinstance(value, bytes):
        return value.decode("latin-1")
    if isinstance(value, (list, tuple)):
        return [jsonable(item) for item in value]
    if isinstance(value, dict):
        return {key: jsonable(item) for key, item in value.items()}
    return value


async def reply(
    send: Send, status: int, body: bytes, headers: list[tuple[bytes, bytes]] | None = None
) -> None:
    """Send a complete response, with a Content-Length unless headers are given."""

    if headers is None:
        headers = [
            (b"content-type", b"application/json"),
            (b"content-length", str(len(body)).encode()),
        ]
    await send({"type": "http.response.start", "status": status, "headers": headers})
    await send({"type": "http.response.body", "body": body})


async def drain(receive: Receive) -> list[dict[str, Any]]:
    """Read every http.request message and return them with their bodies decoded."""

    messages = []
    while True:
        message = await receive()
        messages.append(
            {
                "type": message["type"],
                "size": len(message.get("body", b"")),
                "more_body": message.get("more_body"),
            }
        )
        if message["type"] != "http.request" or not message.get("more_body"):
            return messages


async def lifespan(scope: Scope, receive: Receive, send: Send) -> None:
    message = await receive()
    assert message["type"] == "lifespan.startup"
    scope["state"]["pid"] = os.getpid()
    await send({"type": "lifespan.startup.complete"})

    message = await receive()
    assert message["type"] == "lifespan.shutdown"
    print("conformance: lifespan.shutdown received", flush=True)
    await send({"type": "lifespan.shutdown.complete"})


async def http(scope: Scope, receive: Receive, send: Send) -> None:
    path = scope["path"]

    if path.startswith("/scope"):
        report = {key: jsonable(value) for key, value in scope.items() if key != "extensions"}
        report["extensions"] = sorted(scope["extensions"])
        report["muru"] = jsonable(scope["extensions"]["muru"])
        await reply(send, 200, json.dumps(report).encode())
    elif path == "/messages":
        messages = await drain(receive)
        await reply(send, 200, json.dumps(messages).encode())

        # Only once the response is complete does receive() give the disconnect.

        after = await receive()
        print(f"conformance: after response {after['type']}", flush=True)
    elif path == "/state":
        # The state is a shallow copy per request: a key set here is gone
        # by the next request.

        seen = scope["state"].get("seen")
        scope["state"]["seen"] = True
        await reply(send, 200, json.dumps({"pid": scope["state"]["pid"], "seen": seen}).encode())
    elif path == "/chunked":
        await send(
            {
                "type": "http.response.start",
                "status": 200,
                "headers": [(b"content-type", b"text/plain")],
            }
        )
        for index in range(3):
            await send(
                {"type": "http.response.body", "body": f"{index}".encode(), "more_body": True}
            )
        await send({"type": "http.response.body", "body": b""})
    elif path == "/over-length":
        await reply(send, 200, b"12345", [(b"content-length", b"3")])
    elif path == "/no-body-status":
        await send({"type": "http.response.start", "status": 204, "headers": []})
        await send({"type": "http.response.body", "body": b""})
    elif path == "/head":
        await reply(
            send,
            200,
            b"body for HEAD",
            [(b"content-length", b"13"), (b"x-method", scope["method"].encode())],
        )
    elif path == "/trailers":
        await send({"type": "http.response.start", "status": 200, "headers": [], "trailers": True})
        await send({"type": "http.response.body", "body": b"x", "more_body": True})
        await send({"type": "http.response.trailers", "headers": [(b"x-trailer", b"1")]})
    elif path == "/bad-status-type":
        await send({"type": "http.response.start", "status": "200", "headers": []})
    elif path == "/bad-headers-type":
        await send(
            {
                "type": "http.response.start",
                "status": 200,
                "headers": [("content-type", "text/plain")],
            }
        )
    elif path == "/bad-body-type":
        await send({"type": "http.response.start", "status": 200, "headers": []})
        await send({"type": "http.response.body", "body": "text"})
    elif path == "/unknown-message":
        await send({"type": "http.response.whatever"})
    elif path == "/duplicate-headers":
        headers = [(b"x-dup", b"a"), (b"x-dup", b"b"), (b"content-length", b"2")]
        await reply(send, 200, b"ok", headers)
    else:
        await reply(send, 404, b"{}")


async def websocket(scope: Scope, receive: Receive, send: Send) -> None:
    path = scope["path"]
    first = await receive()
    print(f"conformance: first websocket message {first['type']}", flush=True)

    if path == "/ws/send-before-accept":
        await send({"type": "websocket.send", "text": "early"})
        return
    if path == "/ws/accept-twice":
        await send({"type": "websocket.accept"})
        await send({"type": "websocket.accept"})
        return
    if path == "/ws/close-reason":
        await send({"type": "websocket.accept"})
        await send({"type": "websocket.close", "code": 4042, "reason": "because"})
        return

    await send({"type": "websocket.accept"})
    if path == "/ws/both":
        await send({"type": "websocket.send", "text": "a", "bytes": b"b"})
        return
    if path == "/ws/types":
        # Report the type of each message received, then close.

        kinds = []
        while True:
            message = await receive()
            if message["type"] == "websocket.disconnect":
                kinds.append(f"disconnect:{message['code']}:{message['reason']}")
                break
            kinds.append("text" if message.get("text") is not None else "bytes")
            if message.get("text") == "done" or message.get("bytes") == b"done":
                await send({"type": "websocket.send", "text": json.dumps(kinds)})
                await send({"type": "websocket.close"})
                after = await receive()
                print(f"conformance: after close {after['type']} {after.get('code')}", flush=True)
                return
    elif path == "/ws/scope":
        await send(
            {
                "type": "websocket.send",
                "text": json.dumps(
                    {
                        key: jsonable(value)
                        for key, value in scope.items()
                        if key not in ("extensions", "state")
                    }
                ),
            }
        )
        await send({"type": "websocket.close"})


async def application(scope: Scope, receive: Receive, send: Send) -> None:
    if scope["type"] == "lifespan":
        await lifespan(scope, receive, send)
    elif scope["type"] == "websocket":
        await websocket(scope, receive, send)
    else:
        await http(scope, receive, send)
