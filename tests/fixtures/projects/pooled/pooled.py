"""A project whose mounts have threads of their own (section 6.7).

The default entrypoint is WSGI with two shared threads, `fixed` has a pool
of two threads, `borrowing` reserves one and may borrow one shared thread,
and an ASGI mount sits beside them. Each WSGI application sleeps for the
`ms` query parameter and answers with the thread it ran on.
"""

from __future__ import annotations

import json
import threading
import time
from collections.abc import Callable, Iterable
from typing import Any
from urllib.parse import parse_qs


def _wsgi(name: str) -> Callable[..., Iterable[bytes]]:
    def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
        query = parse_qs(environ.get("QUERY_STRING", ""))
        time.sleep(int(query.get("ms", ["0"])[0]) / 1000)
        body = json.dumps(
            {
                "mount": name,
                "thread": threading.get_ident(),
                "multithread": environ["wsgi.multithread"],
            }
        ).encode()
        start_response(
            "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
        )

        return [body]

    return application


wsgi_app = _wsgi("default")
fixed_app = _wsgi("fixed")
borrowing_app = _wsgi("borrowing")


async def asgi_app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    """The ASGI mount, which makes the worker run an event loop."""

    if scope["type"] == "lifespan":
        while True:
            message = await receive()

            if message["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            elif message["type"] == "lifespan.shutdown":
                await send({"type": "lifespan.shutdown.complete"})
                return

    await send({"type": "http.response.start", "status": 200, "headers": []})
    await send({"type": "http.response.body", "body": b"events"})
