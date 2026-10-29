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
    """An ASGI application, detected from the coroutine function."""

    if scope["type"] == "lifespan":
        return
    body = b"asgi\n"
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"text/plain"), (b"content-length", b"5")],
        }
    )
    await send({"type": "http.response.body", "body": body})


class AsgiCallable:
    """An object whose __call__ is a coroutine function, so it is detected as ASGI."""

    async def __call__(self, scope: dict[str, Any], receive: Any, send: Any) -> None:
        """Answer like asgi_app."""

        await asgi_app(scope, receive, send)


def wsgi_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    """A plain WSGI application, for the argument checks."""

    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "5")])
    return [b"wsgi\n"]


def create_app_with_config(config: dict[str, Any] | None = None) -> Any:
    """A factory whose only parameter has a default, so it takes no required arguments."""

    return create_app()


def three_args(scope: dict[str, Any], receive: Any, send: Any) -> None:
    """A plain function taking an ASGI application's three arguments."""


def star_args(*args: Any, **kwargs: Any) -> Iterable[bytes]:
    """A WSGI application behind a decorator-like signature, which is taken as it is."""

    return wsgi_app(*args, **kwargs)


class KeywordOnly:
    """An object whose __call__ needs a keyword argument no interface passes."""

    def __call__(
        self, environ: dict[str, Any], start_response: Callable[..., Any], *, extra: str
    ) -> Iterable[bytes]:
        return wsgi_app(environ, start_response)


keyword_only = KeywordOnly()
