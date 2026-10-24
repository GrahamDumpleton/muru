"""An ASGI application that raises on a lifespan scope, as ones without lifespan support do."""

from __future__ import annotations

from typing import Any


async def application(scope: dict[str, Any], receive: Any, send: Any) -> None:
    if scope["type"] != "http":
        raise ValueError(f"this application only handles http, not {scope['type']}")
    await send(
        {"type": "http.response.start", "status": 200, "headers": [(b"content-length", b"2")]}
    )
    await send({"type": "http.response.body", "body": b"ok"})
