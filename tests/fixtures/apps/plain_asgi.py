"""A plain ASGI application that reports on its process, for the ASGI tests."""

from __future__ import annotations

import asyncio
import contextvars
import json
import os
import sys
import threading
from collections.abc import Awaitable, Callable, MutableMapping
from typing import Any

Scope = MutableMapping[str, Any]
Receive = Callable[[], Awaitable[dict[str, Any]]]
Send = Callable[[dict[str, Any]], Awaitable[None]]

HELLO = b"Hello, async world!\n"
REQUEST_MARK: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "request_mark", default=None
)


def info() -> dict[str, Any]:
    """Describe the process and thread serving the request."""

    is_gil_enabled = getattr(sys, "_is_gil_enabled", None)
    return {
        "pid": os.getpid(),
        "thread": threading.current_thread().name,
        "threads": threading.active_count(),
        "free_threaded": bool(is_gil_enabled and not is_gil_enabled()),
        "python": sys.version.split()[0],
    }


async def read_body(receive: Receive) -> tuple[bytes, int]:
    """Read the whole request body, returning it and the number of messages."""

    chunks = []
    messages = 0

    while True:
        message = await receive()
        messages += 1
        if message["type"] != "http.request":
            break
        chunks.append(message.get("body", b""))
        if not message.get("more_body", False):
            break

    return b"".join(chunks), messages


async def respond(send: Send, status: int, body: bytes, content_type: str = "text/plain") -> None:
    """Send a complete response with a Content-Length."""

    await send(
        {
            "type": "http.response.start",
            "status": status,
            "headers": [
                (b"content-type", content_type.encode()),
                (b"content-length", str(len(body)).encode()),
            ],
        }
    )
    await send({"type": "http.response.body", "body": body})


async def lifespan(scope: Scope, receive: Receive, send: Send) -> None:
    """Fill the state at startup and announce shutdown on stdout."""

    message = await receive()
    assert message["type"] == "lifespan.startup"
    scope["state"]["started_in"] = os.getpid()
    scope["state"]["counter"] = 0
    await send({"type": "lifespan.startup.complete"})

    message = await receive()
    assert message["type"] == "lifespan.shutdown"
    print("lifespan shutdown complete", flush=True)
    await send({"type": "lifespan.shutdown.complete"})


async def scope_report(scope: Scope, receive: Receive, send: Send) -> None:
    """Answer /info with the scope, the state and what the body held."""

    body, messages = await read_body(receive)

    # Values that JSON can carry, with the byte strings decoded.

    details = {
        **info(),
        "scope": {
            key: value
            for key, value in scope.items()
            if isinstance(value, (str, int, bool, list, tuple, dict))
            and key not in ("state", "extensions")
        },
        "state": scope["state"],
        "extensions": scope["extensions"],
        "body_length": len(body),
        "body_messages": messages,
    }
    details["scope"]["headers"] = [
        [name.decode(), value.decode()] for name, value in scope["headers"]
    ]
    details["scope"]["raw_path"] = scope["raw_path"].decode("latin-1")
    details["scope"]["query_string"] = scope["query_string"].decode("latin-1")

    await respond(send, 200, json.dumps(details).encode(), "application/json")


async def stream(query: dict[str, str], send: Send) -> None:
    """Answer /stream with several chunks, optionally spaced out."""

    count = int(query.get("count", "5"))
    delay = float(query.get("delay", "0"))
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"text/plain")],
        }
    )

    for index in range(count):
        chunk = f"chunk {index}\n".encode()
        await send({"type": "http.response.body", "body": chunk, "more_body": True})
        if delay:
            await asyncio.sleep(delay)

    await send({"type": "http.response.body", "body": b""})


async def events(query: dict[str, str], send: Send) -> None:
    """Answer /events with a server-sent event stream, which is exempt from the timeouts."""

    count = int(query.get("count", "5"))
    delay = float(query.get("delay", "0.1"))
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"text/event-stream")],
        }
    )

    for index in range(count):
        await send(
            {"type": "http.response.body", "body": b"data: %d\n\n" % index, "more_body": True}
        )
        await asyncio.sleep(delay)

    await send({"type": "http.response.body", "body": b""})


async def misbehave(path: str, send: Send) -> None:
    """The routes that break the protocol or fail, for the error tests."""

    if path == "/error":
        raise RuntimeError("deliberate failure before the response")

    if path == "/error-after-start":
        await send(
            {
                "type": "http.response.start",
                "status": 200,
                "headers": [(b"content-type", b"text/plain"), (b"content-length", b"100")],
            }
        )
        await send({"type": "http.response.body", "body": b"partial", "more_body": True})
        raise RuntimeError("deliberate failure after the response started")

    if path == "/no-response":
        return

    if path == "/body-before-start":
        await send({"type": "http.response.body", "body": b"x"})
    elif path == "/double-start":
        await send({"type": "http.response.start", "status": 200, "headers": []})
        await send({"type": "http.response.start", "status": 200, "headers": []})
    elif path == "/body-after-end":
        await respond(send, 200, b"done")
        await send({"type": "http.response.body", "body": b"more"})
    elif path == "/bad-status":
        await send({"type": "http.response.start", "status": 99, "headers": []})
    elif path == "/bad-header":
        headers = [(b"x-a", b"1\r\nx-b: 2")]
        await send({"type": "http.response.start", "status": 200, "headers": headers})
    elif path == "/short":
        await send(
            {
                "type": "http.response.start",
                "status": 200,
                "headers": [(b"content-length", b"10")],
            }
        )
        await send({"type": "http.response.body", "body": b"short"})
    elif path == "/exit":
        raise SystemExit(3)


MISBEHAVING = (
    "/error",
    "/error-after-start",
    "/no-response",
    "/body-before-start",
    "/double-start",
    "/body-after-end",
    "/bad-status",
    "/bad-header",
    "/short",
    "/exit",
)


async def websocket(scope: Scope, receive: Receive, send: Send) -> None:
    """The WebSocket routes: echo, subprotocols, declines, closes and errors."""

    path = scope["path"]
    message = await receive()
    assert message["type"] == "websocket.connect"

    if path == "/ws/decline":
        # Closing before accepting declines the handshake with 403.

        await send({"type": "websocket.close"})
        return

    if path == "/ws/http-decline":
        body = b"no socket for you\n"
        await send(
            {
                "type": "websocket.http.response.start",
                "status": 401,
                "headers": [
                    (b"content-type", b"text/plain"),
                    (b"content-length", str(len(body)).encode()),
                ],
            }
        )
        await send({"type": "websocket.http.response.body", "body": body})
        return

    if path == "/ws/return":
        return

    subprotocol = scope["subprotocols"][0] if scope["subprotocols"] else None
    await send(
        {"type": "websocket.accept", "subprotocol": subprotocol, "headers": [(b"x-ws", b"yes")]}
    )

    if path == "/ws/scope":
        report = {
            key: value.decode("latin-1") if isinstance(value, bytes) else value
            for key, value in scope.items()
            if key not in ("headers", "state", "extensions")
        }
        report["headers"] = [[name.decode(), value.decode()] for name, value in scope["headers"]]
        report["extensions"] = sorted(scope["extensions"])
        report["state"] = scope["state"]
        await send({"type": "websocket.send", "text": json.dumps(report)})
        await send({"type": "websocket.close"})
        return

    if path == "/ws/close-code":
        await send({"type": "websocket.close", "code": 4001, "reason": "custom"})
        return

    if path == "/ws/error":
        raise RuntimeError("deliberate failure on the socket")

    if path == "/ws/send-after-close":
        await send({"type": "websocket.close"})
        await send({"type": "websocket.send", "text": "late"})
        return

    # Echo until the client goes away, then say how it ended on stdout.

    while True:
        message = await receive()

        if message["type"] == "websocket.disconnect":
            print(f"websocket disconnected with {message['code']}", flush=True)
            return

        if message.get("text") is not None:
            await send({"type": "websocket.send", "text": message["text"]})
        else:
            await send({"type": "websocket.send", "bytes": message["bytes"]})


async def application(scope: Scope, receive: Receive, send: Send) -> None:
    """Route by path; see each helper for what the routes do."""

    if scope["type"] == "lifespan":
        await lifespan(scope, receive, send)
        return

    if scope["type"] == "websocket":
        await websocket(scope, receive, send)
        return

    assert scope["type"] == "http"

    # Under a mount the path carries root_path, which the routes here ignore.

    path = scope["path"][len(scope.get("root_path", "")) :]
    query = dict(
        part.split("=", 1) for part in scope["query_string"].decode().split("&") if "=" in part
    )

    if path == "/":
        await respond(send, 200, HELLO)
    elif path.startswith("/info"):
        await scope_report(scope, receive, send)
    elif path == "/echo":
        body, _ = await read_body(receive)
        await respond(send, 200, body, "application/octet-stream")
    elif path == "/stream":
        await stream(query, send)
    elif path == "/events":
        await events(query, send)
    elif path == "/redirect-to":
        # Section 4.10: an internal redirect, with trusted headers for
        # the target.

        headers = [(b"x-muru-redirect", query.get("path", "/info").encode())]

        for item in query.get("header", "").split(","):
            if ":" in item:
                name, value = item.split(":", 1)
                headers.append((f"x-muru-redirect-header-{name}".encode(), value.encode()))

        await send({"type": "http.response.start", "status": 200, "headers": headers})
        await send({"type": "http.response.body", "body": b"redirect\n"})
    elif path == "/sleep":
        await asyncio.sleep(float(query.get("seconds", "0.5")))
        await respond(send, 200, b"slept\n")
    elif path == "/state":
        # The scope's state is a copy per request: changes stay here.

        scope["state"]["counter"] = scope["state"].get("counter", 0) + 1
        await respond(send, 200, json.dumps(scope["state"]).encode(), "application/json")
    elif path == "/context":
        REQUEST_MARK.set(query.get("mark", "set"))
        await asyncio.sleep(0.05)
        await respond(send, 200, (REQUEST_MARK.get() or "none").encode())
    elif path == "/thread":
        name = await asyncio.to_thread(lambda: threading.current_thread().name)
        await respond(send, 200, name.encode())
    elif path == "/large":
        size = int(query.get("size", str(1024 * 1024)))
        await respond(send, 200, b"x" * size, "application/octet-stream")
    elif path == "/disconnect":
        # Wait for the client to go away, then say so on stdout.

        message = await receive()
        while message["type"] != "http.disconnect":
            message = await receive()
        print("client disconnected", flush=True)
    elif path == "/after-response":
        await read_body(receive)
        await respond(send, 200, b"sent")
        message = await receive()
        print(f"after response: {message['type']}", flush=True)
    elif path in MISBEHAVING:
        await misbehave(path, send)
    else:
        await respond(send, 404, b"not found\n")
