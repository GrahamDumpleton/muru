"""Muru runtime library.

This package runs inside every Muru host and worker process. It is also safe
to import outside Muru, where every function returns development defaults, so
applications can use it unconditionally.

``muru.context()`` says where the code runs, ``muru.request_id()``
which request it serves, ``muru.logging`` holds the platform logging
defaults, ``muru.RequestTimeout`` is what the soft timeout raises,
``muru.is_internal()`` tells a request that arrived by internal
redirect, ``muru.uploads`` reaches the files the edge stored for an
upload route, and ``muru.loop()`` and ``muru.asgi`` serve requests
handed between WSGI and ASGI (section 6.14 of the design
specification).
"""

from __future__ import annotations

import asyncio
from collections.abc import Mapping
from importlib.metadata import PackageNotFoundError, version
from typing import Any

from muru import asgi, logging, uploads
from muru._context import Context, context, request_id

__all__ = [
    "Context",
    "HandoffNotEnabled",
    "RequestTimeout",
    "__version__",
    "asgi",
    "context",
    "is_internal",
    "logging",
    "loop",
    "request_id",
    "uploads",
]


def loop() -> asyncio.AbstractEventLoop | None:
    """Return the worker's event loop, for WSGI code that schedules work on it.

    A worker has one when it serves an ASGI application or hands requests
    to one (``handoff = "asgi"``). Code on an application thread reaches
    it with ``call_soon_threadsafe`` or ``asyncio.run_coroutine_threadsafe``.
    None in a worker without one, in the host and outside Muru.
    """

    from muru import _asgi

    return _asgi.current_loop()


def is_internal(request: Mapping[str, Any]) -> bool:
    """Whether a WSGI environ or ASGI scope arrived by internal redirect.

    A request the edge dispatched because an application answered with
    ``X-Muru-Redirect`` (or another redirect form the mount accepts) is
    internal: the environ carries ``REDIRECT_URL``, ``REDIRECT_QUERY_STRING``
    and ``REDIRECT_STATUS``, and the scope's ``extensions["muru"]`` the same
    under ``redirect_url``, ``redirect_query_string`` and ``redirect_status``.
    Outside Muru, and for a client's own request, this is False.
    """

    extensions = request.get("extensions")

    if isinstance(extensions, Mapping):
        muru = extensions.get("muru")
        return bool(muru.get("internal", False)) if isinstance(muru, Mapping) else False

    return bool(request.get("muru.internal", False))


class RequestTimeout(BaseException):
    """Raised in the thread of a request that has run past the soft timeout.

    A ``BaseException`` rather than an ``Exception``, so that handlers which
    catch ``Exception`` to log and carry on do not swallow it: the request
    is over. Code that must clean up uses ``finally`` or catches this class
    by name. Outside Muru it is never raised.
    """


class HandoffNotEnabled(RuntimeError):
    """Raised by a handoff the request's mount does not enable.

    A WSGI application hands requests to ASGI only with
    ``handoff = "asgi"`` in its configuration, and an ASGI application
    hands them to WSGI only with ``handoff = "wsgi"``. The message names
    the setting to add.
    """


# The version is written into the wheel metadata by maturin from Cargo.toml.
# When the package is imported from the source tree without being installed,
# there is no metadata to read.

try:
    __version__ = version("muru")
except PackageNotFoundError:
    __version__ = "0+unknown"
