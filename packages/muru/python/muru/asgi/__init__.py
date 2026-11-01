"""ASGI applications reached by handing requests across (design specification section 6.14).

``register(app)`` runs the lifespan of an application that requests are
handed to but that no mount serves, so its startup and shutdown run in
each worker as a mounted application's do.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable, MutableMapping
from typing import Any, TypeVar

__all__ = ["register"]

App = TypeVar("App", bound=Callable[[MutableMapping[str, Any], Any, Any], Awaitable[None]])


def register(app: App) -> App:
    """Run ``app``'s lifespan in each worker; returns ``app``.

    Called from a ``post_fork`` hook, or at import, the application's
    startup runs once the worker's event loop has started, before the
    worker is ready, and a failed startup fails the worker; its shutdown
    runs when the worker drains, after the requests in flight. Called in
    a worker already serving, from an application thread, the startup
    runs at once. The state its startup fills is copied into the scope of
    every request handed to it. A worker without an event loop does not
    start; outside Muru this does nothing.
    """

    from muru import _asgi

    _asgi.register(app)
    return app
