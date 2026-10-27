"""Memory limits (design specification section 7.7): a worker over the
hard limit leaves routing at once and is replaced; one over the soft
limit for long enough is asked to reclaim and, still over, is recycled;
and each worker's memory reaches the daemon's metrics."""

from __future__ import annotations

import json
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import SHOP, start_daemon

MB = 1024 * 1024


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def recycled(log: str) -> list[str]:
    return [line for line in log.splitlines() if line.startswith("muru: worker.recycled:")]


def test_a_worker_over_the_hard_limit_is_replaced_at_once() -> None:
    server = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=2,
        args=[
            "--memory",
            "measure=rss",
            "--memory",
            "hard_limit=96MB",
            "--memory",
            "check_interval=1s",
        ],
    )

    try:
        first = json.loads(server.get("/info").body)["pid"]
        assert int(server.get("/grow?mb=160").body) >= 160 * MB
        wait_for(lambda: len(recycled(server.log())) >= 1)
        line = recycled(server.log())[0]
        assert "over the hard limit of 96MB" in line and "rss" in line

        # Section 11.7: the event says what the worker held most of.

        assert "; most objects: " in line, line
        wait_for(lambda: json.loads(server.get("/info").body)["pid"] != first)
        assert server.get("/").status == 200
    finally:
        server.stop()


def test_a_worker_over_the_soft_limit_reclaims_and_then_recycles() -> None:
    server = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=2,
        args=[
            "--memory",
            "measure=rss",
            "--memory",
            "soft_limit=96MB",
            "--memory",
            "soft_for=1s",
            "--memory",
            "check_interval=1s",
        ],
    )

    try:
        first = json.loads(server.get("/info").body)["pid"]
        assert server.get("/grow?mb=160").status == 200
        wait_for(lambda: "reclaimed memory: collected" in server.log())
        wait_for(lambda: len(recycled(server.log())) >= 1)
        line = recycled(server.log())[0]
        assert "stayed over the soft limit of 96MB" in line and "after reclaiming" in line
        wait_for(lambda: json.loads(server.get("/info").body)["pid"] != first)
    finally:
        server.stop()


def test_memory_stays_under_the_limits_without_a_recycle() -> None:
    server = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=2,
        args=[
            "--memory",
            "soft_limit=1GB",
            "--memory",
            "hard_limit=2GB",
            "--memory",
            "check_interval=1s",
        ],
    )

    try:
        for _ in range(20):
            assert server.get("/").status == 200

        time.sleep(2.5)
        assert recycled(server.log()) == []
    finally:
        server.stop()


def test_worker_memory_and_requests_reach_the_metrics(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)

    try:
        assert (
            daemon.deploy("shop", SHOP, "--processes", "1", "--memory", "check_interval=1s")
            == "muru: release shop@r1 is ready and serving"
        )
        assert daemon.get("/").status == 200
        wait_for(lambda: "muru_worker_memory_bytes{" in daemon.muructl("metrics").stdout)
        text = daemon.muructl("metrics").stdout
        labels = 'app="shop",release="r1",group="default",worker="0"'
        assert f'muru_worker_memory_bytes{{{labels},measure="rss"}} ' in text
        assert f'muru_worker_memory_bytes{{{labels},measure="uss"}} ' in text
        assert f"muru_worker_requests_total{{{labels}}} " in text
        assert f"muru_worker_requests_in_flight{{{labels}}} 0" in text

        rss = next(
            float(line.split()[-1])
            for line in text.splitlines()
            if line.startswith(f'muru_worker_memory_bytes{{{labels},measure="rss"}}')
        )
        assert rss > 1 * MB
    finally:
        daemon.stop()
