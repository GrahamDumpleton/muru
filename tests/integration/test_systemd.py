"""Running under a service manager (design specification section 13.3).

The daemon reports readiness and stopping with sd_notify when
NOTIFY_SOCKET names a socket, detaches on SIGTERM as `muructl daemon stop`
does, re-reads the [proxy] and [compression] tables on `muructl daemon
reload` or SIGHUP, and
`daemon stop --timeout` bounds the wait for it to exit.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import sys
import tempfile
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import SHOP, Daemon, daemon, start_daemon

__all__ = ["daemon"]

APPS = support.ROOT / "tests" / "fixtures" / "apps"


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.05)


def notifications(receiver: socket.socket, until: str, timeout: float = 30.0) -> list[str]:
    """Datagrams received until one contains `until`."""

    received: list[str] = []
    deadline = time.monotonic() + timeout
    receiver.settimeout(0.5)

    while time.monotonic() < deadline:
        try:
            received.append(receiver.recv(4096).decode())
        except TimeoutError:
            continue

        if until in received[-1]:
            return received

    raise AssertionError(f"no notification with {until!r}: {received}")


def test_readiness_and_stopping_are_notified(tmp_path: Path) -> None:
    # The socket path is short, as a Unix socket path must be.

    directory = Path(tempfile.mkdtemp(prefix="muru-notify-", dir="/tmp"))
    path = directory / "notify.sock"

    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as receiver:
        receiver.bind(str(path))
        daemon = start_daemon(tmp_path, env={"NOTIFY_SOCKET": str(path)})

        try:
            ready = notifications(receiver, "READY=1")
            assert ready[-1].startswith("READY=1\n")
            assert f"STATUS=serving on http://127.0.0.1:{daemon.port}" in ready[-1]
        finally:
            daemon.stop()

        assert "STOPPING=1" in notifications(receiver, "STOPPING=1")

    path.unlink()
    directory.rmdir()


def test_sigterm_detaches_and_the_next_daemon_readopts(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        assert first.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
        pid = json.loads(first.get("/info").body)["pid"]
        os.kill(first.process.pid, signal.SIGTERM)
        first.process.wait(timeout=30)

        # Traffic keeps flowing through the same processes.

        assert first.get("/").status == 200
        assert json.loads(first.get("/info").body)["pid"] == pid

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        events = second.events()
        assert "daemon.detached" in events
        assert "release.adopted" in events
        assert json.loads(second.get("/info").body)["pid"] == pid
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        second.stop(shutdown=True)


def test_reload_applies_the_proxy_table(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, listeners=[(support.free_port(), False)])

    try:
        config = daemon.base / "echo.toml"
        config.write_text(
            f"[app.echo.release.r1]\nsource = '{APPS}'\nentrypoint = 'plain_wsgi:application'\n"
            f"python = '{sys.executable}'\n"
        )
        assert "ready and serving" in daemon.muructl("apply", "-f", str(config)).stdout
        headers = {"X-Forwarded-For": "203.0.113.5"}
        environ = json.loads(daemon.request("/info", headers=headers).body)["environ"]
        assert environ["REMOTE_ADDR"] == "127.0.0.1"

        # Nothing changed yet, and the daemon says so.

        unchanged = daemon.muructl("daemon", "reload").stdout
        assert "the [proxy] and [compression] tables are unchanged" in unchanged, unchanged

        # The table appears in the file; a reload hands it to the edge.

        with (daemon.base / "muru.toml").open("a") as handle:
            handle.write("\n[proxy]\ntrusted = ['loopback']\n")

        reloaded = daemon.muructl("daemon", "reload").stdout
        assert "the [proxy] and [compression] tables changed and the edge has them" in reloaded
        environ = json.loads(daemon.request("/info", headers=headers).body)["environ"]
        assert environ["REMOTE_ADDR"] == "203.0.113.5"
        assert daemon.events().count("config.reloaded") == 2

        # A bad table is refused and the edge keeps what it had.

        with (daemon.base / "muru.toml").open("a") as handle:
            handle.write("trusted = ['not an address']\n")

        refused = daemon.muructl("daemon", "reload", check=False)
        assert refused.returncode != 0
        environ = json.loads(daemon.request("/info", headers=headers).body)["environ"]
        assert environ["REMOTE_ADDR"] == "203.0.113.5"

        # SIGHUP reloads too.

        (daemon.base / "muru.toml").write_text(
            "\n".join(
                line
                for line in (daemon.base / "muru.toml").read_text().splitlines()
                if not line.startswith("trusted") and line != "[proxy]"
            )
            + "\n"
        )
        os.kill(daemon.process.pid, signal.SIGHUP)
        wait_for(lambda: daemon.events().count("config.reloaded") == 3)
        environ = json.loads(daemon.request("/info", headers=headers).body)["environ"]
        assert environ["REMOTE_ADDR"] == "127.0.0.1"
    finally:
        daemon.stop()


def test_stop_takes_a_timeout(daemon: Daemon) -> None:
    completed = daemon.muructl("daemon", "stop", "--shutdown", "--timeout", "60s")
    assert completed.stdout.strip() == "muru: daemon stopped; releases drained and stopped"
    daemon.process.wait(timeout=30)
