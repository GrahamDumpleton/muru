"""A project with an ASGI mount that has limits of its own (section 6.7)
beside a WSGI application. Each application sleeps for the ms= query
parameter, and the mount's WebSockets echo until the client closes.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Callable, Iterable
from typing import Any
from urllib.parse import parse_qs


def wsgi_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    query = parse_qs(environ.get("QUERY_STRING", ""))
    time.sleep(int(query.get("ms", ["0"])[0]) / 1000)
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "4")])
    return [b"wsgi"]


async def asgi_app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    if scope["type"] == "lifespan":
        while True:
            message = await receive()

            if message["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            elif message["type"] == "lifespan.shutdown":
                await send({"type": "lifespan.shutdown.complete"})
                return

    if scope["type"] == "websocket":
        message = await receive()
        assert message["type"] == "websocket.connect"
        await send({"type": "websocket.accept"})

        while True:
            message = await receive()

            if message["type"] == "websocket.disconnect":
                return

            await send({"type": "websocket.send", "text": f"echo:{message.get('text', '')}"})

    query = parse_qs(scope["query_string"].decode())
    await asyncio.sleep(int(query.get("ms", ["0"])[0]) / 1000)
    await send({"type": "http.response.start", "status": 200, "headers": []})
    await send({"type": "http.response.body", "body": b"asgi"})
