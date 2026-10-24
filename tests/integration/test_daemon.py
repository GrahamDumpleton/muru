"""The daemon (design specification sections 9.1, 9.5, 10 and 13.3).

A daemon runs with its own state, runtime and log directories; releases are
deployed from source directories with the blue/green strategy, traffic
moves when the new release is ready and the previous one drains, rollback
restores the previous release, and every state change is an event.
"""

from __future__ import annotations

import http.client
import json
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path

import pytest
from support import server as support

SHOP = support.ROOT / "tests" / "fixtures" / "projects" / "shop"
FACTORY = support.ROOT / "tests" / "fixtures" / "projects" / "factory"


@dataclass
class Daemon:
    """A daemon running in the foreground with directories of its own."""

    process: subprocess.Popen[bytes]
    port: int
    base: Path
    run_dir: Path
    log_path: Path

    def muructl(self, *arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        """Run a muructl command against this daemon."""

        muructl = support.muructl_path()
        assert muructl is not None
        completed = subprocess.run(
            [str(muructl), *arguments, "--socket", str(self.run_dir / "admin.sock")],
            capture_output=True,
            text=True,
            check=False,
            timeout=180,
        )
        if check and completed.returncode != 0:
            raise RuntimeError(
                f"muructl {' '.join(arguments)} failed:\n{completed.stdout}{completed.stderr}"
            )
        return completed

    def deploy(self, app: str, source: Path, *arguments: str) -> str:
        """Deploy and return the line muructl printed."""

        completed = self.muructl("deploy", app, str(source), "--python", sys.executable, *arguments)
        return completed.stdout.strip()

    def get(self, path: str = "/") -> support.Response:
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)
        try:
            connection.request("GET", path)
            raw = connection.getresponse()
            return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
        finally:
            connection.close()

    def releases(self, app: str = "") -> list[dict[str, str]]:
        """The release table as muructl prints it, parsed by column."""

        lines = self.muructl("release", "list", *([app] if app else [])).stdout.splitlines()
        rows = []
        for line in lines[1:]:
            parts = line.split()
            if not parts or "@" not in parts[0]:
                continue
            rows.append(
                {
                    "release": parts[0],
                    "state": parts[1],
                    "serving": "yes" if "yes" in parts[2:3] else "",
                }
            )
        return rows

    def events(self) -> list[str]:
        return [line.split(None, 4)[3] for line in self.muructl("events").stdout.splitlines()]

    def log(self) -> str:
        return self.log_path.read_text(errors="replace")

    def stop(self) -> None:
        """Stop the daemon through its API, or kill it if that fails."""

        if self.process.poll() is None:
            completed = self.muructl("daemon", "stop", check=False)
            try:
                self.process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
            if completed.returncode != 0:
                raise RuntimeError(
                    f"muructl daemon stop failed: {completed.stdout}{completed.stderr}"
                )


def start_daemon(base: Path) -> Daemon:
    """Start a daemon in the foreground with directories under `base`."""

    muructl = support.muructl_path()
    if muructl is None:
        support.unavailable("muructl is not built (cargo build -p muructl)")
    if not support.extension_available():
        support.unavailable("the muru extension is not built into this interpreter (just dev)")

    # The runtime directory holds the admin socket, whose path is limited
    # to about a hundred bytes, so it goes under /tmp rather than pytest's
    # deep temporary directory.
    run_dir = Path(tempfile.mkdtemp(prefix="muru-run-", dir="/tmp"))
    port = support.free_port()
    log_path = base / "daemon.out"
    with log_path.open("wb") as handle:
        process = subprocess.Popen(
            [
                str(muructl),
                "daemon",
                "run",
                "--state-dir",
                str(base / "state"),
                "--run-dir",
                str(run_dir),
                "--log-dir",
                str(base / "logs"),
                "--listen",
                f"127.0.0.1:{port}",
            ],
            stdout=handle,
            stderr=subprocess.STDOUT,
        )
    daemon = Daemon(process, port, base, run_dir, log_path)

    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"the daemon exited early:\n{daemon.log()}")
        if (run_dir / "admin.sock").exists() and daemon.muructl(
            "status", check=False
        ).returncode == 0:
            return daemon
        time.sleep(0.1)
    raise RuntimeError(f"the daemon did not answer:\n{daemon.log()}")


@pytest.fixture
def daemon(tmp_path: Path) -> Iterator[Daemon]:
    instance = start_daemon(tmp_path)
    try:
        yield instance
    finally:
        instance.stop()


def test_status_before_any_release(daemon: Daemon) -> None:
    status = daemon.muructl("status").stdout

    assert f"http://127.0.0.1:{daemon.port}" in status
    assert "apps     0 (0 releases)" in status
    assert daemon.get("/").status == 503


def test_deploy_serves_and_records_the_release(daemon: Daemon) -> None:
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"

    response = daemon.get("/")
    assert response.status == 200
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]
    assert "shop" in daemon.muructl("app", "list").stdout
    assert daemon.events()[-3:] == ["release.created", "release.ready", "traffic.switched"]


def test_second_deploy_moves_traffic_without_failed_requests(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    pid_before = json.loads(daemon.get("/info").body)["pid"]

    # Requests keep flowing while the deploy happens, on kept-alive
    # connections as real clients use (and so the test does not run out
    # of ephemeral ports).
    results: list[int] = []
    stop = threading.Event()

    def hammer() -> None:
        connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=30)
        try:
            while not stop.is_set():
                try:
                    connection.request("GET", "/")
                    response = connection.getresponse()
                    response.read()
                    results.append(response.status)
                except (OSError, http.client.HTTPException):
                    results.append(0)
                    connection.close()
                    connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=30)
                time.sleep(0.001)
        finally:
            connection.close()

    threads = [threading.Thread(target=hammer) for _ in range(4)]
    for thread in threads:
        thread.start()
    try:
        assert (
            daemon.deploy("shop", SHOP, "--processes", "2")
            == "muru: release shop@r2 is ready and serving"
        )
        time.sleep(0.5)
    finally:
        stop.set()
        for thread in threads:
            thread.join()

    assert results, "no requests were made"
    assert set(results) == {200}, f"statuses seen during the deploy: {sorted(set(results))}"
    assert json.loads(daemon.get("/info").body)["pid"] != pid_before
    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "ready", "serving": "yes"},
        {"release": "shop@r1", "state": "stopped", "serving": ""},
    ]


def test_rollback_restores_the_previous_release(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP, "--release", "second")

    assert (
        daemon.muructl("rollback", "shop").stdout.strip() == "muru: rolled back to shop@r1 (ready)"
    )
    assert daemon.get("/").status == 200
    assert daemon.releases("shop") == [
        {"release": "shop@second", "state": "stopped", "serving": ""},
        {"release": "shop@r1", "state": "ready", "serving": "yes"},
    ]
    assert "APP                  CURRENT      PREVIOUS" in daemon.muructl("app", "list").stdout
    assert "shop                 r1           second" in daemon.muructl("app", "list").stdout


def test_rollback_without_a_previous_release_is_refused(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    completed = daemon.muructl("rollback", "shop", check=False)

    assert completed.returncode != 0
    assert "no previous release" in completed.stderr


def test_drain_takes_the_release_out_of_traffic(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)

    assert daemon.muructl("drain", "shop@r1").stdout.strip() == "muru: release shop@r1 is stopped"
    assert daemon.get("/").status == 503
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "stopped", "serving": ""}]


def test_failed_deploy_keeps_the_previous_release_serving(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    completed = daemon.muructl(
        "deploy",
        "shop",
        str(SHOP),
        "--python",
        sys.executable,
        "--entrypoint",
        "shop.wsgi:missing",
        check=False,
    )

    assert completed.returncode != 0
    assert "host exited" in completed.stderr or "failed" in completed.stderr
    assert daemon.get("/").status == 200
    assert daemon.releases("shop")[0] == {"release": "shop@r2", "state": "failed", "serving": ""}
    assert "release.failed" in daemon.events()


def test_settings_come_from_the_table_and_flags(daemon: Daemon) -> None:
    # The shop project's pyproject.toml sets processes and threads; the
    # factory project's muru.toml names a factory entrypoint.
    daemon.deploy("shop", SHOP)
    assert "2x3" in daemon.muructl("release", "list", "shop").stdout

    daemon.deploy("factory", FACTORY, "--threads", "2")
    listing = daemon.muructl("release", "list", "factory").stdout
    assert "1x2" in listing
    assert daemon.releases("factory")[0]["state"] == "ready"


def test_release_wait_and_no_wait(daemon: Daemon) -> None:
    line = daemon.deploy("shop", SHOP, "--no-wait")
    assert line.startswith("muru: release shop@r1 recorded")

    waited = daemon.muructl("release", "wait", "shop@r1", "--timeout", "60").stdout.strip()
    assert waited == "muru: release shop@r1 is ready"
    assert daemon.get("/").status == 200


def test_a_dead_host_is_started_again(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    pid = json.loads(daemon.get("/info").body)["pid"]
    host_pid = next(
        int(line.split("[")[1].split("]")[0])
        for line in daemon.log().splitlines()
        if line.startswith("muru.host[")
    )
    os.kill(host_pid, signal.SIGKILL)

    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if "release.restarting" in daemon.events() and daemon.get("/").status == 200:
            break
        time.sleep(0.2)
    assert daemon.get("/").status == 200
    assert json.loads(daemon.get("/info").body)["pid"] != pid
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]


def test_current_release_is_started_again_after_a_restart(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    try:
        first.deploy("shop", SHOP)
        assert first.get("/").status == 200
    finally:
        first.stop()

    second = start_daemon(tmp_path)
    try:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and second.get("/").status != 200:
            time.sleep(0.2)
        assert second.get("/").status == 200
        assert second.releases("shop") == [
            {"release": "shop@r1", "state": "ready", "serving": "yes"}
        ]
    finally:
        second.stop()


def test_background_daemon_start_status_stop(tmp_path: Path) -> None:
    muructl = support.muructl_path()
    if muructl is None or not support.extension_available():
        support.unavailable("muructl or the extension is not built")
    run_dir = Path(tempfile.mkdtemp(prefix="muru-run-", dir="/tmp"))
    port = support.free_port()
    location = [
        "--state-dir",
        str(tmp_path / "state"),
        "--run-dir",
        str(run_dir),
        "--log-dir",
        str(tmp_path / "logs"),
    ]

    started = subprocess.run(
        [str(muructl), "daemon", "start", "--listen", f"127.0.0.1:{port}", *location],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )
    try:
        assert started.returncode == 0, started.stdout + started.stderr
        assert "daemon started" in started.stdout
        again = subprocess.run(
            [str(muructl), "up", *location], capture_output=True, text=True, check=False, timeout=60
        )
        assert "already running" in again.stdout
        status = subprocess.run(
            [str(muructl), "daemon", "status", *location],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        assert f"http://127.0.0.1:{port}" in status.stdout
    finally:
        stopped = subprocess.run(
            [str(muructl), "daemon", "stop", *location],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )
    assert "daemon stopped" in stopped.stdout, stopped.stdout + stopped.stderr
    assert (tmp_path / "logs" / "daemon.log").is_file()
