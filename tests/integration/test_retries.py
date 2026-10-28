"""Requests sent again (design specification section 4.8).

A worker that cannot be connected to, or that closes its connection before
the request head is written to it, has read nothing of the request, so the
edge sends it to another worker. A worker that has read the head is never
asked again.
"""

from __future__ import annotations

import json
import os
import signal
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def pids_serving(get: Callable[[str], support.Response], count: int = 20) -> set[int]:
    """The worker pids that answer `count` requests, which also leaves an
    idle pooled connection to each worker."""

    return {int(json.loads(get("/info").body)["pid"]) for _ in range(count)}


def test_a_request_a_worker_never_read_goes_to_another(tmp_path: Path) -> None:
    log = tmp_path / "access.log"
    server = support.start(
        "plain_wsgi:application",
        workers=2,
        threads=2,
        args=["--access-log", str(log)],
    )

    try:
        pids = pids_serving(server.get)
        assert len(pids) == 2, pids
        victim = min(pids)

        # Once the kernel has closed what the killed worker held (a moment
        # after the kill), its pooled connections fail to write and its
        # socket refuses connections until the host has started it again.
        # Every request still gets its answer from the other worker.

        os.kill(victim, signal.SIGKILL)
        time.sleep(0.1)

        for _ in range(60):
            response = server.get("/info")
            assert response.status == 200, response.body
            assert int(json.loads(response.body)["pid"]) != victim
            time.sleep(0.01)

        assert server.wait_for_log("is ready", timeout=10)
        served = pids_serving(server.get)
        assert len(served) == 2 and victim not in served, served
    finally:
        server.stop()

    records = [json.loads(line) for line in log.read_text().splitlines()]
    assert all(record["status"] == 200 for record in records), [
        record for record in records if record["status"] != 200
    ]
    assert any(record["retried"] for record in records), "no request was sent again"


def test_retries_are_counted(daemon: Daemon) -> None:
    assert daemon.deploy("shop", SHOP, "--processes", "2") == (
        "muru: release shop@r1 is ready and serving"
    )
    pids = pids_serving(daemon.get)
    assert len(pids) == 2, pids
    victim = min(pids)
    os.kill(victim, signal.SIGKILL)
    time.sleep(0.1)

    for _ in range(60):
        response = daemon.get("/info")
        assert response.status == 200, response.body
        time.sleep(0.01)

    metrics = daemon.muructl("metrics").stdout
    assert 'muru_request_retries_total{app="shop",release="r1",group="default"} ' in metrics, (
        metrics
    )
