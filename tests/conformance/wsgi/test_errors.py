"""wsgi.errors and the standard streams reach the server log."""

from __future__ import annotations

import pytest
from support.server import Server, text

AREA = "/errors"


@pytest.mark.parametrize(
    ("case", "markers"),
    [
        ("write", ["MARKER_ERRORS_WRITE"]),
        ("writelines", ["MARKER_ERRORS_LINE1", "MARKER_ERRORS_LINE2"]),
        ("stdout", ["MARKER_STDOUT"]),
        ("stderr", ["MARKER_STDERR"]),
    ],
)
def test_streams_reach_the_log(server: Server, case: str, markers: list[str]) -> None:
    server.mark_log()
    assert text(server.get(f"{AREA}/{case}")) == "written"
    for marker in markers:
        assert server.wait_for_log(marker), marker
