"""Client classes, site modes and method restrictions (sections 4.14 and
4.15), and overlays (section 9.10): `wsgi_app` and `asgi_app` answer
every request with the class and mode they were given, the method and,
for `wsgi_app`, the release that served it, as JSON.
"""

from __future__ import annotations

import json
from collections.abc import Awaitable, Callable, Iterable
from typing import Any

import muru


def wsgi_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    body = json.dumps(
        {
            "class": environ.get("muru.client_class"),
            "mode": environ.get("muru.mode"),
            "method": environ["REQUEST_METHOD"],
            "release": muru.context().release,
        }
    ).encode()
    start_response(
        "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
    )
    return [body]


async def asgi_app(
    scope: dict[str, Any],
    receive: Callable[[], Awaitable[dict[str, Any]]],
    send: Callable[[dict[str, Any]], Awaitable[None]],
) -> None:
    if scope["type"] == "lifespan":
        while True:
            message = await receive()

            if message["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            elif message["type"] == "lifespan.shutdown":
                await send({"type": "lifespan.shutdown.complete"})
                return

    muru = scope.get("extensions", {}).get("muru", {})
    body = json.dumps(
        {"class": muru.get("client_class"), "mode": muru.get("mode"), "method": scope["method"]}
    ).encode()
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"application/json")],
        }
    )
    await send({"type": "http.response.body", "body": body})
