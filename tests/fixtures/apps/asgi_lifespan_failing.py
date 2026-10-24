"""An ASGI application whose lifespan startup fails, which must stop the worker."""

from __future__ import annotations

from typing import Any


async def application(scope: dict[str, Any], receive: Any, send: Any) -> None:
    if scope["type"] == "lifespan":
        await receive()
        await send({"type": "lifespan.startup.failed", "message": "no database"})
        return

    await send({"type": "http.response.start", "status": 200, "headers": []})
    await send({"type": "http.response.body", "body": b"never"})
