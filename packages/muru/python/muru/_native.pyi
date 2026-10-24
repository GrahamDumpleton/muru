"""Type stubs for the muru._native extension (crates/muru-py)."""

from collections.abc import Callable, Iterator
from typing import Any

def version() -> str:
    """Return the version of the extension."""

def serve(
    socket_path: str,
    app: Callable[..., Any],
    threads: int = 4,
    multiprocess: bool = True,
    event_loop: Any | None = None,
    runner: Callable[[Any, Channel], None] | None = None,
    concurrency: int = 0,
    on_drain: Callable[[], Any] | None = None,
    on_exit: Callable[[], Any] | None = None,
    on_reopen_logs: Callable[[], Any] | None = None,
    lifeline: int | None = None,
    mounts: list[tuple[str, Callable[..., Any], Callable[[Any, Channel], None] | None]] = [],
    report: int | None = None,
) -> None:
    """Serve the worker link on a Unix socket until SIGTERM or SIGINT.

    multiprocess is what wsgi.multiprocess reports: False only for a
    single-process development run. For an ASGI application, event_loop is
    the worker's running asyncio loop, runner is scheduled on it with the
    scope and channel of each request, and concurrency is how many requests
    the loop takes at once. on_drain runs when the stop signal arrives,
    before the requests in flight finish, on_exit after they have, and
    on_reopen_logs whenever SIGUSR1 arrives. lifeline is a file descriptor
    whose end of file also stops the server: the read end of a pipe whose
    write end the host holds, so a worker never outlives its host. mounts
    are the named mounts as (name, application, runner) tuples, the runner
    None for a WSGI mount; event_loop is needed when any mount is ASGI.
    """

def resolve_future(future: Any, value: Any) -> None:
    """Complete an asyncio future with a value unless it is already done."""

class Channel:
    """The link side of one ASGI request: receive and send for the application."""

    def receive(self) -> Any:
        """Return a future for the next http.request or http.disconnect message."""

    def send(self, message: dict[str, Any]) -> Any | None:
        """Queue a response message; returns a future to await when the writer is behind."""

    def finish(self) -> None:
        """Record that the application returned, failing a response it never completed."""

    def fail(self, detail: str) -> None:
        """Fail the response because the application raised, with the formatted traceback."""

class Input:
    """The wsgi.input stream."""

    def read(self, size: int = -1) -> bytes: ...
    def readline(self, limit: int = -1) -> bytes: ...
    def readlines(self, hint: int = -1) -> list[bytes]: ...
    def __iter__(self) -> Iterator[bytes]: ...
    def __next__(self) -> bytes: ...
    def close(self) -> None: ...

class FileWrapper:
    """The wsgi.file_wrapper class: iterate a file-like object in blocks."""

    def __init__(self, filelike: Any, block_size: int = 8192) -> None: ...
    def __iter__(self) -> Iterator[bytes]: ...
    def __next__(self) -> bytes: ...
    def close(self) -> None: ...

class StartResponse:
    """The start_response callable, which also provides write."""

    def __call__(
        self,
        status: str,
        headers: list[tuple[str, str]],
        exc_info: Any = None,
    ) -> Callable[[bytes], None]: ...
    def write(self, data: bytes) -> None: ...
