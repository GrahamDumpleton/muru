"""An ASGI application for uvicorn, which reports the server it runs under."""

from __future__ import annotations

import json
import os
from collections.abc import Awaitable, Callable
from typing import Any


async def app(
    scope: dict[str, Any],
    receive: Callable[[], Awaitable[dict[str, Any]]],
    send: Callable[[dict[str, Any]], Awaitable[None]],
) -> None:
    if scope["type"] != "http":
        return

    body = json.dumps({"server": "uvicorn", "pid": os.getpid(), "path": scope["path"]}).encode()
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
