"""A factory entrypoint: `create_app()` builds the application.

The response reports which process called the factory, so a test can tell
a per-worker call (section 6.8 default) from a call in the host.
"""

from __future__ import annotations

import json
import os
from collections.abc import Callable, Iterable
from typing import Any

CALLS = 0


def create_app() -> Callable[..., Iterable[bytes]]:
    """Build the application, remembering the process doing so."""

    global CALLS
    CALLS += 1
    created_in = os.getpid()

    def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
        body = json.dumps(
            {"created_in": created_in, "serving": os.getpid(), "calls": CALLS}
        ).encode()
        start_response(
            "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
        )
        return [body]

    return application


async def asgi_app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    """An ASGI application, which phase 1 refuses with a clear message."""


class AsgiCallable:
    """An object whose __call__ is a coroutine function."""

    async def __call__(self, scope: dict[str, Any], receive: Any, send: Any) -> None:
        """Do nothing."""
