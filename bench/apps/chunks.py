"""Tiny chunks: 1000 yields of 16 bytes, measures per-chunk cost and coalescing."""

from __future__ import annotations

from collections.abc import Callable, Iterable
from typing import Any

COUNT = 1000
HEADERS = [("Content-Type", "text/plain")]


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    start_response("200 OK", HEADERS)
    return (b"%015d\n" % index for index in range(COUNT))
