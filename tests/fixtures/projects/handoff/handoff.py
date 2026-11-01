"""A project whose WSGI application hands requests to ASGI (section 6.14).

The default mount has handoff = "asgi": it hands a WebSocket to an echo
application, and an HTTP request, part of whose body it has read, to an
application that reports what it received. The plain mount has no
handoff, so its attempt raises HandoffNotEnabled.
"""

from __future__ import annotations

import asyncio
import contextvars
import json
import os
from collections.abc import Callable, Iterable
from typing import Any

import muru
from muru import HandoffNotEnabled

request_value: contextvars.ContextVar[str] = contextvars.ContextVar("request_value", default="")


async def report(
    scope: dict[str, Any], receive: Callable[..., Any], send: Callable[..., Any]
) -> None:
    """Answer with the rest of the body, the scope's state and the context."""

    body = b""

    while True:
        message = await receive()
        body += message.get("body", b"")

        if not message.get("more_body"):
            break

    details = {
        "type": scope["type"],
        "path": scope["path"],
        "body": body.decode(),
        "state": scope["state"],
        "context": request_value.get(),
    }
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"application/json")],
        }
    )
    await send({"type": "http.response.body", "body": json.dumps(details).encode()})


async def echo(
    scope: dict[str, Any], receive: Callable[..., Any], send: Callable[..., Any]
) -> None:
    """Accept a WebSocket and echo each message, with the state's greeting first."""

    await receive()
    await send({"type": "websocket.accept"})
    await send({"type": "websocket.send", "text": scope["state"].get("greeting", "")})

    while True:
        message = await receive()

        if message["type"] == "websocket.disconnect":
            return

        if message.get("text") is not None:
            await send({"type": "websocket.send", "text": message["text"]})
        else:
            await send({"type": "websocket.send", "bytes": message["bytes"]})


async def with_lifespan(
    scope: dict[str, Any], receive: Callable[..., Any], send: Callable[..., Any]
) -> None:
    """Fill the lifespan state at startup; answer a request with its state and request id."""

    if scope["type"] == "lifespan":
        while True:
            message = await receive()

            if message["type"] == "lifespan.startup":
                scope["state"]["started_in"] = os.getpid()
                await send({"type": "lifespan.startup.complete"})
            else:
                print("with_lifespan: shutdown", flush=True)
                await send({"type": "lifespan.shutdown.complete"})
                return

    await receive()
    details = {
        "state": scope["state"],
        "request_id": muru.request_id(),
        "pid": os.getpid(),
    }
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"application/json")],
        }
    )
    await send({"type": "http.response.body", "body": json.dumps(details).encode()})


async def answer() -> int:
    return 42


def respond(start_response: Callable[..., Any], status: str, text: str) -> Iterable[bytes]:
    start_response(status, [("Content-Type", "text/plain")])
    return [text.encode()]


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    bridge = environ["muru.wsgi_to_asgi"]
    path = environ["PATH_INFO"]

    if path == "/ws":
        if not bridge.is_websocket:
            return respond(start_response, "426 Upgrade Required", "websocket required\n")

        bridge.scope["state"]["greeting"] = "hello from wsgi"
        return bridge.handoff(echo)

    if path == "/ws-decline":
        return respond(start_response, "403 Forbidden", f"declined {bridge.is_websocket}\n")

    if path == "/body":
        first = environ["wsgi.input"].read(5)
        bridge.scope["state"]["first"] = first.decode()
        request_value.set("set in wsgi")
        return bridge.handoff(report)

    if path == "/discarded":
        # The response a framework view returns after a handoff is ignored.

        bridge.handoff(report)
        return respond(start_response, "500 Internal Server Error", "ignored\n")

    if path == "/after-write":
        write = start_response("200 OK", [("Content-Type", "text/plain")])
        write(b"started; ")

        try:
            bridge.handoff(report)
        except RuntimeError as error:
            return [str(error).encode()]

        return [b"handed off\n"]

    if path == "/twice":
        bridge.handoff(report)

        try:
            bridge.handoff(report)
        except RuntimeError as error:
            print(f"second handoff: {error}", flush=True)

        return []

    if path == "/registered":
        bridge.scope["state"]["from_wsgi"] = muru.request_id()
        return bridge.handoff(with_lifespan)

    if path == "/loop":
        loop = muru.loop()
        assert loop is not None
        result = asyncio.run_coroutine_threadsafe(answer(), loop).result(5)
        return respond(start_response, "200 OK", f"loop {result}\n")

    return respond(start_response, "200 OK", f"wsgi {bridge.handed_off}\n")


def plain(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    bridge = environ["muru.wsgi_to_asgi"]

    try:
        return bridge.handoff(report)
    except HandoffNotEnabled as error:
        return respond(start_response, "409 Conflict", f"{type(error).__name__}: {error}\n")
