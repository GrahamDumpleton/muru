"""Logs under the daemon (design specification sections 11.2 and 11.3).

Each release's host writes its own and its workers' tagged lines to a
file in the log directory, and the edge writes the access log there as
JSON lines that name the application, release, host, group and mount.
muructl logs shows or follows either, and muructl logs reopen makes every
writer open its file again by name after an external rotation, passing
the request on to the workers' on_reopen_logs hook when asked.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import threading
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import SHOP, Daemon, daemon

__all__ = ["daemon"]


def wait_for(predicate: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.05)


def test_release_output_is_shown_and_followed(daemon: Daemon) -> None:
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    assert daemon.request("/first").status == 200

    # The host's own lines and the workers' relayed lines, the latter
    # tagged with the worker and stream, come back in order.

    wait_for(lambda: "shop: served /first" in daemon.muructl("logs", "shop").stdout)
    lines = daemon.muructl("logs", "shop").stdout.splitlines()
    assert any(line.startswith("muru.host[") for line in lines)
    assert any(line.endswith(".stdout: shop: served /first") for line in lines), lines
    assert len(daemon.muructl("logs", "shop@r1", "--lines", "2").stdout.splitlines()) == 2

    # Following prints what is written after the command started.

    muructl = support.muructl_path()
    assert muructl is not None
    following = subprocess.Popen(
        [
            str(muructl),
            "logs",
            "shop",
            "--follow",
            "--lines",
            "0",
            "--socket",
            str(daemon.run_dir / "admin.sock"),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    seen: list[str] = []

    def read() -> None:
        assert following.stdout is not None
        for line in following.stdout:
            seen.append(line.rstrip("\n"))

    reader = threading.Thread(target=read, daemon=True)
    reader.start()

    try:
        time.sleep(0.5)
        assert daemon.request("/second").status == 200
        wait_for(lambda: any(line.endswith("shop: served /second") for line in seen))
        assert not any("served /first" in line for line in seen), (
            "lines before the follow are not shown"
        )
    finally:
        following.terminate()
        following.wait(timeout=10)


def test_access_log_names_the_release(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    assert daemon.request("/catalogue?page=2").status == 200
    wait_for(lambda: "/catalogue" in daemon.muructl("logs", "--access").stdout)

    records = [
        json.loads(line)
        for line in daemon.muructl("logs", "--access", "--json").stdout.splitlines()
    ]
    record = next(record for record in records if record["target"] == "/catalogue?page=2")
    assert (record["app"], record["release"], record["group"], record["mount"]) == (
        "shop",
        "r1",
        "default",
        "default",
    )
    assert record["host"] == "127.0.0.1" and record["snapshot"] >= 1
    assert record["worker"] in (0, 1)

    compact = daemon.muructl("logs", "--access", "--lines", "1").stdout.strip()
    assert " 200 GET /catalogue?page=2 " in compact and " release=shop@r1 " in compact, compact
    assert compact.endswith(f" id={record['request_id']}")

    # The release's log and the access log are what the log directory holds.

    logs = daemon.base / "logs"
    assert (logs / "apps" / "shop" / "r1.log").exists() and (logs / "access.log").exists()


def test_logs_reopen_after_an_external_rotation(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    assert daemon.request("/before").status == 200
    logs = daemon.base / "logs"
    release_log = logs / "apps" / "shop" / "r1.log"
    access_log = logs / "access.log"
    wait_for(lambda: "served /before" in release_log.read_text())
    wait_for(lambda: "/before" in access_log.read_text())

    # logrotate's way: rename, then ask every writer to reopen by name.

    os.rename(release_log, release_log.with_suffix(".log.1"))
    os.rename(access_log, access_log.with_suffix(".log.1"))
    reopened = daemon.muructl("logs", "reopen", "--workers").stdout.strip()
    assert reopened == "muru: reopened the log files: the edge and 1 host and their workers"
    assert daemon.request("/after").status == 200
    wait_for(lambda: release_log.exists() and "served /after" in release_log.read_text())
    wait_for(lambda: access_log.exists() and "/after" in access_log.read_text())
    assert "served /before" not in release_log.read_text()
    assert "/before" not in access_log.read_text()

    # The workers ran on_reopen_logs, and the event stream says so.

    wait_for(lambda: release_log.read_text().count("shop: reopened logs") == 2)
    assert "reopened" in release_log.read_text(), "the host says it reopened its file"
    assert "logs.reopened" in daemon.events()


def test_logs_for_an_unknown_target_are_refused(daemon: Daemon) -> None:
    refused = daemon.muructl("logs", "nothing", check=False)
    assert refused.returncode != 0
    assert "nothing" in refused.stderr

    daemon.deploy("shop", SHOP)
    refused = daemon.muructl("logs", "shop@r9", check=False)
    assert refused.returncode != 0 and "r9" in refused.stderr

    # Without a release, a bare application name has nothing to show.

    daemon.muructl("drain", "shop@r1")
    refused = daemon.muructl("logs", "shop", check=False)
    assert refused.returncode != 0 and "no current release" in refused.stderr


def test_sigusr1_reopens_the_daemons_own_log(tmp_path: Path) -> None:
    """A background daemon writes to daemon.log and reopens it on SIGUSR1."""

    muructl = support.muructl_path()
    assert muructl is not None
    base = tmp_path
    run_dir = Path("/tmp") / f"muru-run-{os.getpid()}-{time.monotonic_ns()}"
    port = support.free_port()
    environment = {**os.environ, "MURU_SOCKET": str(run_dir / "admin.sock")}
    started = subprocess.run(
        [
            str(muructl),
            "daemon",
            "start",
            "--state-dir",
            str(base / "state"),
            "--run-dir",
            str(run_dir),
            "--log-dir",
            str(base / "logs"),
            "--listen",
            f"127.0.0.1:{port}",
        ],
        capture_output=True,
        text=True,
        check=True,
        timeout=60,
        env=environment,
    )
    assert "daemon started" in started.stdout
    daemon_log = base / "logs" / "daemon.log"
    pid = int((run_dir / "daemon.pid").read_text())

    try:
        wait_for(daemon_log.exists)
        os.rename(daemon_log, daemon_log.with_suffix(".log.1"))
        os.kill(pid, signal.SIGUSR1)
        wait_for(lambda: daemon_log.exists() and "reopened the log files" in daemon_log.read_text())
    finally:
        subprocess.run(
            [str(muructl), "daemon", "stop", "--shutdown"],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
            env=environment,
        )
