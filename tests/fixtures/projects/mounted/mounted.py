"""A project whose muru.toml declares mounts (section 6.7).

The default entrypoint is WSGI; an ASGI mount and a second WSGI mount sit
beside it in the same workers. Each answers with which it is and the paths
it saw, and the ASGI mount echoes WebSocket messages under /asgi/ws.
"""

from __future__ import annotations

import json
import os
from collections.abc import Callable, Iterable
from typing import Any

from muru import context


def _wsgi(name: str) -> Callable[..., Iterable[bytes]]:
    def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
        body = json.dumps(
            {
                "mount": name,
                "interface": "wsgi",
                "script_name": environ["SCRIPT_NAME"],
                "path_info": environ["PATH_INFO"],
                "context_interface": context().interface,
                "pid": os.getpid(),
            }
        ).encode()
        start_response(
            "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
        )

        return [body]

    return application


wsgi_app = _wsgi("default")
reports_app = _wsgi("reports")

STARTED: dict[str, Any] = {}


async def asgi_app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    """The ASGI mount: lifespan, an HTTP report, and a WebSocket echo."""

    if scope["type"] == "lifespan":
        while True:
            message = await receive()

            if message["type"] == "lifespan.startup":
                scope["state"]["started_in"] = os.getpid()
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

    body = json.dumps(
        {
            "mount": "asgi",
            "interface": "asgi",
            "root_path": scope["root_path"],
            "path": scope["path"],
            "started_in": scope["state"].get("started_in"),
            "pid": os.getpid(),
        }
    ).encode()
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [
                (b"content-type", b"application/json"),
                (b"content-length", str(len(body)).encode()),
            ],
        }
    )
    await send({"type": "http.response.body", "body": body})
