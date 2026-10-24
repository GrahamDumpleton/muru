"""Ports of `muructl run` (design specification section 13.1): --bind and
--port, `--port auto`, and the message when a port is in use."""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support


def run_muructl(*args: str) -> tuple[subprocess.Popen[bytes], Path]:
    """Start muructl run for the plain fixture with the given extra arguments."""

    muructl = support.muructl_path()
    if muructl is None or not support.extension_available():
        pytest.skip("muructl and the extension must be built")
    log = Path(tempfile.mkstemp(prefix="muru-ports-", suffix=".log")[1])

    with log.open("wb") as handle:
        process = subprocess.Popen(
            [
                str(muructl),
                "run",
                "plain_wsgi:application",
                "--chdir",
                str(support.FIXTURES),
                "--python",
                sys.executable,
                "--dashboard",
                "off",
                *args,
            ],
            stdout=handle,
            stderr=subprocess.STDOUT,
        )

    return process, log


def wait_for_line(log: Path, pattern: str, process: subprocess.Popen[bytes]) -> re.Match[str]:
    deadline = time.monotonic() + 30

    while time.monotonic() < deadline:
        match = re.search(pattern, log.read_text(errors="replace"))
        if match:
            return match
        if process.poll() is not None:
            raise AssertionError(f"muructl exited with {process.returncode}:\n{log.read_text()}")
        time.sleep(0.05)

    raise AssertionError(f"no match for {pattern!r} in:\n{log.read_text()}")


@pytest.fixture
def auto_ports() -> Iterator[tuple[subprocess.Popen[bytes], Path]]:
    process, log = run_muructl("--bind", "127.0.0.1", "--port", "auto", "--https")

    try:
        yield process, log
    finally:
        process.terminate()
        process.wait(timeout=20)


def test_port_auto_picks_free_ports_and_prints_them(
    auto_ports: tuple[subprocess.Popen[bytes], Path],
) -> None:
    process, log = auto_ports

    match = wait_for_line(
        log, r"serving .* on http://127\.0\.0\.1:(\d+) and https://127\.0\.0\.1:(\d+) ", process
    )
    http_port, https_port = int(match.group(1)), int(match.group(2))
    assert http_port not in (0, 8080) and https_port not in (0, 8443)
    assert http_port != https_port
    server = support.Server(http_port, process, log, 1, 4)
    assert server.get("/").status == 200


def test_a_port_in_use_is_reported_with_its_holder(server: support.Server) -> None:
    with pytest.raises(RuntimeError) as failure:
        support.start("plain_wsgi:application", args=["--port", str(server.port)])
    message = str(failure.value)
    assert f"port {server.port} on 127.0.0.1 is in use" in message
    assert "--port" in message
    if shutil.which("lsof") or shutil.which("ss"):
        assert re.search(r"in use by \S+ \(pid \d+\)", message), message


def test_a_https_port_in_use_names_the_https_flag(server: support.Server) -> None:
    with pytest.raises(RuntimeError) as failure:
        support.start(
            "plain_wsgi:application",
            args=["--https", "--https-port", str(server.port), "--port", str(support.free_port())],
        )

    assert "--https-port" in str(failure.value)


@pytest.fixture(scope="module")
def server() -> Iterator[support.Server]:
    instance = support.start("plain_wsgi:application", workers=1, threads=1)

    try:
        yield instance
    finally:
        instance.stop()
