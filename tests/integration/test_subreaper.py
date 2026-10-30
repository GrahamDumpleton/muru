"""The host as a subreaper (design specification section 6.10): a process a
worker started and left behind is reparented to the host rather than to
init, given a grace period, stopped and collected, whether the worker was
killed by a hard timeout or went with the host, so nothing of a release
lingers or sits as a zombie."""

from __future__ import annotations

import http.client
import threading
import time
from collections.abc import Callable
from pathlib import Path

import pytest
from support import server as support

pytestmark = pytest.mark.requires_linux


def wait_for(predicate: Callable[[], bool], timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def process_state(pid: int) -> str | None:
    """The state letter of a process from /proc, Z for a zombie; None once it is gone."""

    try:
        stat = Path(f"/proc/{pid}/stat").read_bytes()
    except OSError:
        return None

    return stat[stat.rfind(b")") + 1 :].split()[0].decode()


def spawn_in_the_background(server: support.Server, path: str) -> threading.Thread:
    """Request a path whose response may never come, on a thread of its own."""

    def request() -> None:
        connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=60)

        try:
            connection.request("GET", path)
            connection.getresponse().read()
        except (OSError, http.client.HTTPException):
            pass
        finally:
            connection.close()

    thread = threading.Thread(target=request)
    thread.start()
    return thread


def test_what_a_killed_worker_left_is_stopped_and_collected(tmp_path: Path) -> None:
    server = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=2,
        args=[
            "--timeout",
            "hard=1s",
            "--timeout",
            "hard_drain_grace=1s",
            "--timeout",
            "shutdown=1s",
        ],
    )
    pidfile = tmp_path / "child.pid"
    thread: threading.Thread | None = None

    try:
        # The request starts a child and then runs past the hard timeout,
        # which kills the worker and leaves the child behind.

        thread = spawn_in_the_background(server, f"/spawn?seconds=30&pidfile={pidfile}")
        wait_for(pidfile.exists)
        child = int(pidfile.read_text())
        assert process_state(child) not in (None, "Z")
        wait_for(lambda: "worker.timeout_killed" in server.log(), timeout=30)

        # The child is the host's now: it gets the shutdown timeout to
        # exit, is told to stop, and is collected, leaving no zombie.

        wait_for(lambda: process_state(child) is None)
        assert f"left pid {child} running" in server.log()
        assert f"pid {child}, left by worker" not in server.log(), "SIGTERM should have done"

        # The release itself is unaffected.

        assert server.get("/").status == 200
    finally:
        server.stop()

        if thread is not None:
            thread.join(timeout=10)


def test_what_the_workers_leave_running_goes_with_the_host(tmp_path: Path) -> None:
    server = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=2,
        args=["--timeout", "shutdown=1s"],
    )
    pidfile = tmp_path / "child.pid"

    try:
        # The request returns with its child still running in the worker,
        # holding the worker's pipes open.

        assert server.get(f"/spawn?pidfile={pidfile}").status == 200
        child = int(pidfile.read_text())
        assert process_state(child) not in (None, "Z")
    finally:
        # Stopping the server stops the worker, whose child comes to the
        # host and is stopped in turn, so the host does not wait on the
        # pipe the child holds.

        started = time.monotonic()
        server.stop()

    assert server.process.returncode == 0
    assert time.monotonic() - started < 15
    wait_for(lambda: process_state(child) is None, timeout=5)
