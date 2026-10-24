"""One server for the whole WSGI conformance suite."""

from __future__ import annotations

from collections.abc import Iterator

import pytest
from support.server import Server, start

WORKERS = 2
THREADS = 2


@pytest.fixture(scope="package")
def server() -> Iterator[Server]:
    instance = start("wsgi_conformance:application", workers=WORKERS, threads=THREADS)
    try:
        yield instance
    finally:
        instance.stop()
