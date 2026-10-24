"""One server for the HTTP/1.1 parsing suite."""

from __future__ import annotations

from collections.abc import Iterator

import pytest
from support import server as support


@pytest.fixture(scope="package")
def server() -> Iterator[support.Server]:
    instance = support.start(
        "wsgi_conformance:application",
        workers=1,
        threads=2,
        args=["--dashboard", "off", "--max-body-size", "10MB"],
    )

    try:
        yield instance
    finally:
        instance.stop()
