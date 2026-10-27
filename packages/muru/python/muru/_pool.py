"""The frame the runtime's application threads wait in (section 11.7).

Each thread of a WSGI pool runs its whole life inside `serve_jobs`, which
calls back into the runtime to wait for and run requests. A thread the
runtime made would otherwise have no Python frame at all while it waits,
and PEP 768's reader cannot read a thread parked at its interpreter's
base frame, so a worker with idle threads could not be read from outside.
The function runs no bytecode while the runtime has the thread, so
nothing raised into the thread can land in it.
"""

from __future__ import annotations

from collections.abc import Callable


def serve_jobs(serve: Callable[[], None]) -> None:
    """Run the runtime's loop for one application thread."""

    serve()
