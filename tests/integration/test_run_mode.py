"""Development mode is said out loud, and public addresses need --allow-public
(design specification section 13.1)."""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest
from support import server as support


def test_development_mode_is_announced() -> None:
    server = support.start("plain_wsgi:application", workers=1)

    try:
        # Both lines are printed once the listeners are up, which can be
        # a moment after the first request is answered on a busy machine.

        assert server.wait_for_log("muru: development mode: error pages show tracebacks")
        assert server.wait_for_log("muru: dashboard on http://127.0.0.1:")
        response = server.get("/error", headers={"Accept": "application/json"})
        assert response.status == 500
        assert "Traceback" in json.loads(response.body)["traceback"]
    finally:
        server.stop()


def start_raw(*args: str) -> tuple[subprocess.Popen[bytes], Path]:
    muructl = support.muructl_path()
    if muructl is None or not support.extension_available():
        pytest.skip("muructl and the extension must be built")
    log = Path(tempfile.mkstemp(prefix="muru-mode-", suffix=".log")[1])

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
                "--processes",
                "1",
                *args,
            ],
            stdout=handle,
            stderr=subprocess.STDOUT,
        )

    return process, log


def test_a_public_address_is_refused_without_allow_public() -> None:
    process, log = start_raw("--bind", "0.0.0.0", "--port", "auto")
    assert process.wait(timeout=30) != 0
    text = log.read_text()
    assert "serves loopback addresses only" in text
    assert "--allow-public" in text
    assert "daemon" in text


def test_allow_public_turns_development_behaviours_off() -> None:
    process, log = start_raw("--bind", "0.0.0.0", "--port", "auto", "--allow-public")

    try:
        deadline = time.monotonic() + 30

        while time.monotonic() < deadline:
            match = re.search(r"serving .* on http://0\.0\.0\.0:(\d+) ", log.read_text())
            if match:
                break
            assert process.poll() is None, log.read_text()
            time.sleep(0.05)
        else:
            raise AssertionError(log.read_text())

        server = support.Server(int(match.group(1)), process, log, 1, 4)
        text = server.log()
        assert "muru: --allow-public: development behaviours are off" in text
        assert "development mode" not in text
        assert "dashboard on" not in text
        info = json.loads(server.get("/info").body)["environ"]
        assert info["muru.max_body_size"] == 10 * 1024 * 1024
        response = server.get("/error", headers={"Accept": "application/json"})
        assert response.status == 500
        assert "traceback" not in json.loads(response.body)
    finally:
        process.terminate()
        process.wait(timeout=20)
