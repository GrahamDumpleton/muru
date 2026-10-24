"""Deploys, rollbacks, drains and daemon restarts under sustained load with
zero failed requests: the exit criterion of phase 2 (design specification
sections 14.5 and 15.2).

Clients keep requesting on kept-alive connections while each operation
happens, and every request must come back 200. The short scenarios run in
the ordinary suite; the long cycle carries the slow marker.
"""

from __future__ import annotations

import json
import time
from pathlib import Path

import pytest
from support.daemon import SHOP, Daemon, start_daemon
from support.daemon import daemon as daemon_fixture
from support.load import Load

daemon = daemon_fixture


def pid_of(daemon: Daemon) -> int:
    return int(json.loads(daemon.get("/info").body)["pid"])


def test_deploys_under_load(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    first = pid_of(daemon)

    # Three deploys in a row: the second changes the process model, the
    # third stops the first release as the retention slot is reused.

    with Load(daemon.port) as load:
        assert daemon.deploy("shop", SHOP) == "muru: release shop@r2 is ready and serving"
        assert (
            daemon.deploy("shop", SHOP, "--processes", "1", "--threads", "2")
            == "muru: release shop@r3 is ready and serving"
        )
        assert daemon.deploy("shop", SHOP) == "muru: release shop@r4 is ready and serving"
        time.sleep(0.5)

    load.check("three deploys")
    assert pid_of(daemon) != first
    assert [row["state"] for row in daemon.releases("shop")] == [
        "ready",
        "retained",
        "stopped",
        "stopped",
    ]


def test_rollbacks_under_load(daemon: Daemon) -> None:
    # One worker each, so the pid says which release answered.

    daemon.deploy("shop", SHOP, "--processes", "1")
    first = pid_of(daemon)
    daemon.deploy("shop", SHOP, "--processes", "1")
    assert pid_of(daemon) != first

    # Back to the warm first release, forward again, and back once more.

    with Load(daemon.port) as load:
        for expected in ("shop@r1", "shop@r2", "shop@r1"):
            assert (
                daemon.muructl("rollback", "shop").stdout.strip()
                == f"muru: rolled back to {expected} (ready)"
            )
            time.sleep(0.3)

    load.check("three rollbacks")
    assert pid_of(daemon) == first
    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "retained", "serving": ""},
        {"release": "shop@r1", "state": "ready", "serving": "yes"},
    ]


def test_cold_rollback_under_load(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP, "--retain", "0")
    assert daemon.releases("shop")[1]["state"] == "stopped"

    # The previous release is stopped, so the rollback starts it again
    # before traffic moves; the release it leaves is retained warm.

    with Load(daemon.port) as load:
        assert (
            daemon.muructl("rollback", "shop").stdout.strip()
            == "muru: rolled back to shop@r1 (ready)"
        )
        time.sleep(0.3)

    load.check("a cold rollback")
    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "retained", "serving": ""},
        {"release": "shop@r1", "state": "ready", "serving": "yes"},
    ]


def test_draining_the_retained_release_under_load(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)

    with Load(daemon.port) as load:
        assert (
            daemon.muructl("drain", "shop@r1").stdout.strip() == "muru: release shop@r1 is stopped"
        )
        time.sleep(0.3)

    load.check("a drain of the retained release")
    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "ready", "serving": "yes"},
        {"release": "shop@r1", "state": "stopped", "serving": ""},
    ]


def test_daemon_restart_under_load(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        first.deploy("shop", SHOP, "--processes", "1")
        pid = pid_of(first)

        # The daemon detaches and a new one re-adopts the edge and the
        # host; then the new daemon deploys through the adopted edge.

        with Load(first.port) as load:
            assert "detached" in first.muructl("daemon", "stop").stdout
            first.process.wait(timeout=30)
            time.sleep(0.5)
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
            assert pid_of(second) == pid
            assert second.deploy("shop", SHOP) == "muru: release shop@r2 is ready and serving"
            time.sleep(0.3)

        load.check("a daemon restart and a deploy after it")
        assert pid_of(second) != pid
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        second.stop(shutdown=True)


@pytest.mark.slow
def test_sustained_cycle_of_deploys_and_rollbacks(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    deadline = time.monotonic() + 60
    operations = 0

    with Load(daemon.port, clients=8) as load:
        while time.monotonic() < deadline:
            assert daemon.deploy("shop", SHOP).endswith("is ready and serving")
            assert daemon.muructl("rollback", "shop").stdout.startswith("muru: rolled back to")
            assert daemon.muructl("rollback", "shop").stdout.startswith("muru: rolled back to")
            operations += 3

    load.check(f"{operations} operations over a minute", at_least=5000)
    print(load.summary())
