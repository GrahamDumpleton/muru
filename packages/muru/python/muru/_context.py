"""Where the current code runs: application, release, group, worker and model.

``muru.context()`` returns development defaults outside Muru, so applications
and hooks can call it unconditionally (design specification section 6.9).
"""

from __future__ import annotations

from dataclasses import dataclass

__all__ = ["Context", "context"]


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


def _set(current: Context) -> None:
    """Install the context for this process. Called by the host and workers."""

    global _current
    _current = current
