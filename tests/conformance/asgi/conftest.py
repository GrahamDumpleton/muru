"""One server for the whole ASGI conformance suite."""

from __future__ import annotations

from collections.abc import Iterator

import pytest
from support.server import Server, start


@pytest.fixture(scope="package")
def server() -> Iterator[Server]:
    instance = start(
        "asgi_conformance:application",
        workers=1,
        threads=None,
        # No dashboard: its default port would clash with the servers of other suites.
        args=["--dashboard", "off"],
    )
    try:
        yield instance
    finally:
        instance.stop()
