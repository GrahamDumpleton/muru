"""Muru runtime library.

This package runs inside every Muru host and worker process. It is also safe
to import outside Muru, where every function returns development defaults, so
applications can use it unconditionally.

``muru.context()`` says where the code runs, ``muru.logging`` holds the
platform logging defaults, ``muru.RequestTimeout`` is what the soft
timeout raises and ``muru.is_internal()`` tells a request that arrived
by internal redirect; the rest of the API in section 6.9 of the design
specification arrives with the features it belongs to.
"""

from __future__ import annotations

from collections.abc import Mapping
from importlib.metadata import PackageNotFoundError, version
from typing import Any

from muru import logging
from muru._context import Context, context

__all__ = ["Context", "RequestTimeout", "__version__", "context", "is_internal", "logging"]


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


# The version is written into the wheel metadata by maturin from Cargo.toml.
# When the package is imported from the source tree without being installed,
# there is no metadata to read.

try:
    __version__ = version("muru")
except PackageNotFoundError:
    __version__ = "0+unknown"
