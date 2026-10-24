"""Hello world: the smallest possible response, measures per-request overhead."""

from __future__ import annotations

from collections.abc import Callable, Iterable
from typing import Any

BODY = b"Hello, world!\n"
HEADERS = [("Content-Type", "text/plain"), ("Content-Length", str(len(BODY)))]


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    start_response("200 OK", HEADERS)
    return [BODY]
