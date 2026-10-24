"""HTTP/2 conformance with h2spec, when `just bootstrap-conformance` has fetched it."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
from pathlib import Path

import pytest
from support.server import ROOT, Server

# Cases hyper's HTTP/2 implementation can fail, for the same reason: after
# an invalid preface or a client GOAWAY it closes the socket, sometimes with
# input still unread, which the peer then sees as a reset rather than the
# clean close h2spec waits for. Whether that happens depends on timing, so
# these cases are allowed to fail. Neither affects a well-behaved client.

ALLOWED_FAILURES = {
    ("http2/3.5", "Sends invalid connection preface"),
    ("http2/7", "Sends a GOAWAY frame with unknown error code"),
    ("generic/3.8", "Sends a GOAWAY frame"),
}


def h2spec_path() -> str | None:
    candidate = os.environ.get("H2SPEC") or str(ROOT / "target" / "tools" / "h2spec")
    if Path(candidate).is_file():
        return candidate
    return shutil.which("h2spec")


pytestmark = pytest.mark.skipif(
    h2spec_path() is None, reason="h2spec is not available (just bootstrap-conformance)"
)


def test_h2spec(server: Server, tmp_path: Path) -> None:
    binary = h2spec_path()
    assert binary is not None
    report = tmp_path / "h2spec.xml"
    completed = subprocess.run(
        [
            binary,
            "-h",
            "127.0.0.1",
            "-p",
            str(server.port),
            "--timeout",
            "5",
            "-P",
            "/response/list",
            "-j",
            str(report),
        ],
        capture_output=True,
        text=True,
        check=False,
        timeout=300,
    )
    assert report.is_file(), completed.stdout + completed.stderr
    failed, total = parse_report(report.read_text())
    assert total > 100, "h2spec ran fewer cases than expected"
    unexpected = failed - ALLOWED_FAILURES
    assert not unexpected, f"h2spec failures beyond the allowed ones: {sorted(unexpected)}"


CASE = re.compile(
    r'<testcase package="([^"]*)" classname="([^"]*)"[^>]*>(.*?)</testcase>', re.DOTALL
)


def parse_report(text: str) -> tuple[set[tuple[str, str]], int]:
    """Return the failed (package, name) pairs and the case count.

    The report is JUnit-like XML, but h2spec does not escape the quotes in
    some case names, so it is read with a pattern rather than a parser.
    """

    failed = set()
    total = 0
    for match in CASE.finditer(text):
        total += 1
        if "<failure" in match.group(3) or "<error" in match.group(3):
            failed.add((match.group(1), match.group(2)))
    return failed, total
