"""Large body: one 1 MiB response, measures bulk transfer and copies."""

from __future__ import annotations

from collections.abc import Callable, Iterable
from typing import Any

BODY = b"x" * (1024 * 1024)
HEADERS = [("Content-Type", "application/octet-stream"), ("Content-Length", str(len(BODY)))]


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    start_response("200 OK", HEADERS)
    return [BODY]
