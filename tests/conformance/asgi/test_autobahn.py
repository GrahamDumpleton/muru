"""WebSocket conformance with the Autobahn test suite, which runs as a container.

The fuzzing client in the crossbario/autobahn-testsuite image drives the
plain ASGI fixture's echo endpoint through every case of the suite and
writes a report; the test reads the report and fails on any case that is
not OK. The compression cases are informational, since the edge does not
negotiate permessage-deflate.
"""

from __future__ import annotations

import json
import platform
import shutil
import subprocess
from pathlib import Path

import pytest
from support import server as support

IMAGE = "crossbario/autobahn-testsuite:latest"

# Outcomes that are not failures: NON-STRICT is the suite's own leniency,
# UNIMPLEMENTED and INFORMATIONAL are extension cases the edge does not
# negotiate.
ACCEPTABLE = {"OK", "NON-STRICT", "UNIMPLEMENTED", "INFORMATIONAL"}

# Cases that echo messages of 4 to 16 MiB, over the 1 MiB limit of section
# 4.18, which the edge answers with close code 1009 rather than an echo.
# The cases up to 1 MiB in the same sections run.
OVER_THE_LIMIT = ["9.1.4", "9.1.5", "9.1.6", "9.2.4", "9.2.5", "9.2.6", "9.3.*", "9.4.*"]

pytestmark = pytest.mark.requires_docker


def docker_available() -> bool:
    binary = shutil.which("docker")
    if binary is None:
        return False
    return subprocess.run([binary, "info"], capture_output=True, check=False).returncode == 0


def host_address() -> str:
    """The address at which a container reaches this host's listeners."""

    return "127.0.0.1" if platform.system() == "Linux" else "host.docker.internal"


def test_autobahn(tmp_path: Path) -> None:
    if not docker_available():
        pytest.fail("the Autobahn suite needs a running container runtime")

    # The fixture must listen beyond loopback for the container to reach
    # it on macOS, which --allow-public permits.
    instance = support.start(
        "plain_asgi:application",
        workers=1,
        threads=None,
        args=["--dashboard", "off", "--allow-public"],
        host="0.0.0.0",
    )
    try:
        config = tmp_path / "fuzzingclient.json"
        config.write_text(
            json.dumps(
                {
                    "outdir": "/reports",
                    "servers": [{"url": f"ws://{host_address()}:{instance.port}/ws/echo"}],
                    "cases": ["*"],
                    "exclude-cases": OVER_THE_LIMIT,
                    "exclude-agent-cases": {},
                }
            )
        )
        reports = tmp_path / "reports"
        reports.mkdir()
        command = ["docker", "run", "--rm"]
        if platform.system() == "Linux":
            command.append("--network=host")
        command += [
            "-v",
            f"{config}:/config/fuzzingclient.json:ro",
            "-v",
            f"{reports}:/reports",
            IMAGE,
            "wstest",
            "--mode",
            "fuzzingclient",
            "--spec",
            "/config/fuzzingclient.json",
        ]
        completed = subprocess.run(
            command, capture_output=True, text=True, check=False, timeout=1800
        )
    finally:
        instance.stop()

    index = reports / "index.json"
    assert index.is_file(), completed.stdout + completed.stderr
    results = next(iter(json.loads(index.read_text()).values()))
    assert len(results) > 300, "the suite ran fewer cases than expected"
    failed = {
        case: outcome["behavior"]
        for case, outcome in results.items()
        if outcome["behavior"] not in ACCEPTABLE or outcome["behaviorClose"] not in ACCEPTABLE
    }
    assert not failed, f"Autobahn cases not OK: {failed}"
