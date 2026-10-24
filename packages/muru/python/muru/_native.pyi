"""Type stubs for the muru._native extension (crates/muru-py)."""

from collections.abc import Callable, Iterator
from typing import Any

def version() -> str:
    """Return the version of the extension."""

def serve(socket_path: str, app: Callable[..., Any], threads: int = 4) -> None:
    """Serve the worker link on a Unix socket until SIGTERM or SIGINT."""

class Input:
    """The wsgi.input stream."""

    def read(self, size: int = -1) -> bytes: ...
    def readline(self, limit: int = -1) -> bytes: ...
    def readlines(self, hint: int = -1) -> list[bytes]: ...
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
