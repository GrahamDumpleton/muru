"""Upgrading the daemon in place (design specification section 13.4):
`muructl daemon upgrade` re-executes the daemon from the installed binary,
keeping its pid and its children, and the new image re-adopts the hosts
and replaces the edge, all under load with zero failed requests."""

from __future__ import annotations

import json
import os
import signal
import subprocess
import time
from pathlib import Path

from support.daemon import SHOP, Daemon, start_daemon
from support.daemon import daemon as daemon_fixture
from support.load import Load

daemon = daemon_fixture


def edge_pid_of(daemon: Daemon) -> int:
    status = daemon.muructl("status").stdout
    return int(next(line.split()[2] for line in status.splitlines() if line.startswith("edge")))


def wait_until_gone(pid: int, timeout: float = 30.0) -> None:
    """Wait for a process to exit and be reaped."""

    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return

        time.sleep(0.1)

    raise AssertionError(f"process {pid} is still there")


def test_upgrade_keeps_the_pid_and_replaces_the_edge_under_load(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP, "--processes", "1")
    worker = json.loads(daemon.get("/info").body)["pid"]
    edge_before = edge_pid_of(daemon)

    with Load(daemon.port) as load:
        completed = daemon.muructl("daemon", "upgrade")
        time.sleep(0.5)

    load.check("the upgrade", retries_at_most=4)
    lines = completed.stdout.splitlines()
    assert lines[0].startswith(f"muru: daemon (pid {daemon.process.pid}, version ")
    assert "re-executing from" in lines[0]
    assert lines[1].startswith("muru: daemon upgraded to version ")
    assert f"(pid {daemon.process.pid}); edge pid {edge_before} replaced by pid" in lines[1]

    # The same process, a new edge, the same host: nothing restarted.

    assert daemon.process.poll() is None
    edge_after = edge_pid_of(daemon)
    assert edge_after != edge_before
    assert json.loads(daemon.get("/info").body)["pid"] == worker
    events = daemon.events()
    assert "daemon.upgraded" in events
    assert "release.adopted" in events
    assert "edge.replaced" in events
    assert "daemon.started" not in events[events.index("daemon.upgraded") :]

    # The old edge finished its connections and is gone, reaped by the
    # daemon it is now a child of.

    wait_until_gone(edge_before)

    # A second upgrade works the same way, and deploys go on after it.

    completed = daemon.muructl("daemon", "upgrade")
    assert f"edge pid {edge_after} replaced by pid" in completed.stdout
    assert daemon.process.poll() is None
    assert daemon.deploy("shop", SHOP, "--processes", "1") == (
        "muru: release shop@r2 is ready and serving"
    )
    assert json.loads(daemon.get("/info").body)["pid"] != worker
    assert daemon.events().count("daemon.upgraded") == 2


def test_upgraded_daemon_can_detach_and_shut_down_cleanly(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        first.deploy("shop", SHOP)
        first.muructl("daemon", "upgrade")
        edge = edge_pid_of(first)

        # The upgraded daemon detaches like any other, and the next one
        # takes over the replaced edge and the host.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        assert first.get("/").status == 200
        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        assert edge_pid_of(second) == edge
        assert second.releases("shop") == [
            {"release": "shop@r1", "state": "ready", "serving": "yes"}
        ]
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        second.stop(shutdown=True)

    listing = subprocess.run(
        ["ps", "-ax", "-o", "command="], capture_output=True, text=True, check=False
    ).stdout
    assert not any(first.run_dir.name in line for line in listing.splitlines())


def test_respawned_edge_is_readopted_by_the_next_daemon(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        first.deploy("shop", SHOP)
        edge = edge_pid_of(first)
        os.kill(edge, signal.SIGKILL)

        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and "edge.restarted" not in first.events():
            time.sleep(0.2)
        assert "edge.restarted" in first.events()
        respawned = edge_pid_of(first)
        assert respawned != edge
        assert first.get("/").status == 200

        # The record names the respawned edge, so a detach and a restart
        # take it over rather than binding the listener again.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        assert edge_pid_of(second) == respawned
        assert "edge" not in [event for event in second.events() if event == "edge.restarted"]
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        second.stop(shutdown=True)
