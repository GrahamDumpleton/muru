"""Where the current code runs: application, release, group, worker and model.

``muru.context()`` returns development defaults outside Muru, so applications
and hooks can call it unconditionally (design specification section 6.9).
"""

from __future__ import annotations

from contextvars import ContextVar
from dataclasses import dataclass

__all__ = ["Context", "context", "request_id"]


@dataclass(frozen=True)
class Context:
    """What the host or worker is running, as handed to hooks and ``muru.context()``."""

    app: str = "default"
    """The application name."""

    release: str = "dev"
    """The release identifier."""

    group: str = "default"
    """The worker group."""

    worker_id: int | None = None
    """The worker's index within its group, or None in the host."""

    interface: str = "wsgi"
    """The interface of the default mount: wsgi or asgi."""

    process_model: str = "threaded"
    """threaded, process-per-request, async, async-with-sync-pool or free-threaded."""


_current: Context | None = None


def context() -> Context:
    """Return the current context, or development defaults outside Muru."""

    return _current if _current is not None else Context()


# Section 6.9: the request being served, set by the bridges in the
# request's own context, so it follows the request into tasks and
# handoffs and is None between requests.

_request_id: ContextVar[str | None] = ContextVar("muru.request_id", default=None)


def request_id() -> str | None:
    """Return the id of the request being served, or None outside one."""

    return _request_id.get()


def _set(current: Context) -> None:
    """Install the context for this process. Called by the host and workers."""

    global _current
    _current = current
