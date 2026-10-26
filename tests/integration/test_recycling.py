"""Worker replacement (design specification sections 7.6 and 7.7): a
worker past max_requests or max_lifetime is replaced by a fresh one on a
new socket with no request failed, and a worker whose request runs past
the hard timeout is taken out of routing, its stacks recorded, and
replaced. The host tells its supervisor over the control socket, under
muructl run and under the daemon alike."""

from __future__ import annotations

import http.client
import json
import re
import threading
import time
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon
from support.load import Load


def wait_for(predicate: Callable[[], bool], timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def replacements(log: str) -> list[str]:
    """The worker.recycled and worker.timeout_killed lines muructl printed."""

    return [
        line
        for line in log.splitlines()
        if line.startswith("muru: worker.") and "replaced by pid" in line
    ]


@pytest.fixture
def recycled() -> Iterator[support.Server]:
    instance = support.start(
        "plain_wsgi:application", workers=2, threads=2, args=["--max-requests", "20"]
    )

    try:
        yield instance
    finally:
        instance.stop()


def test_workers_are_replaced_after_max_requests_with_no_request_failed(
    recycled: support.Server,
) -> None:
    before = set(recycled.worker_pids().values())

    with Load(recycled.port) as load:
        wait_for(lambda: len(replacements(recycled.log())) >= 4)
        time.sleep(0.5)

    load.check("recycling by request count")
    lines = replacements(recycled.log())
    assert all("worker.recycled" in line and "max_requests 20" in line for line in lines)
    assert "worker 0 (pid" in "".join(lines) and "worker 1 (pid" in "".join(lines)

    # Every request is still answered, by workers none of which served
    # the first ones.

    info = json.loads(recycled.get("/info").body)
    assert info["pid"] not in before
    assert recycled.get("/").status == 200


def test_workers_are_replaced_after_max_lifetime() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=2, args=["--max-lifetime", "1s"]
    )

    try:
        first = json.loads(server.get("/info").body)["pid"]
        wait_for(lambda: len(replacements(server.log())) >= 2, timeout=30)
        assert "max_lifetime 1 s" in replacements(server.log())[0]
        assert json.loads(server.get("/info").body)["pid"] != first
    finally:
        server.stop()


def test_a_request_past_the_hard_timeout_replaces_its_worker() -> None:
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

    try:
        stuck_pid = json.loads(server.get("/info").body)["pid"]
        outcome: list[str] = []

        def stuck() -> None:
            connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=60)

            try:
                connection.request("GET", "/sleep?seconds=30")
                outcome.append(f"status {connection.getresponse().status}")
            except (OSError, http.client.HTTPException) as error:
                outcome.append(type(error).__name__)
            finally:
                connection.close()

        thread = threading.Thread(target=stuck)
        thread.start()

        # The other thread keeps answering while the stuck one is found,
        # and the worker is replaced with its stacks in the log.

        assert server.get("/").status == 200
        wait_for(lambda: "worker.timeout_killed" in server.log(), timeout=30)
        log = server.log()
        assert re.search(r"past the hard timeout of 1 s", log)
        assert "plain_wsgi.py" in log and "in application" in log, "no stack dump"

        # Section 11.7: the event says where the stuck request was, from
        # the stacks taken before the worker went.

        wait_for(lambda: "muru: worker.timeout_killed:" in server.log(), timeout=30)
        assert re.search(
            r"muru: worker\.timeout_killed: .*; at plain_wsgi\.py line \d+ in application",
            server.log(),
        ), server.log()
        thread.join(timeout=30)
        assert outcome and outcome[0] != "status 200", outcome

        wait_for(lambda: json.loads(server.get("/info").body)["pid"] != stuck_pid)
        assert server.get("/sleep?seconds=0.1").status == 200
    finally:
        server.stop()


def test_an_event_loop_that_stalls_replaces_its_worker() -> None:
    server = support.start(
        "plain_asgi:application",
        workers=1,
        args=[
            "--timeout",
            "loop_stall=1s",
            "--timeout",
            "hard_drain_grace=1s",
            "--timeout",
            "shutdown=1s",
        ],
    )

    try:
        stalled_pid = json.loads(server.get("/info").body)["pid"]
        thread = threading.Thread(target=lambda: server.get("/block?seconds=30"), daemon=True)
        thread.start()

        # The loop beats no more, so the native watchdog tells the host,
        # which replaces the worker as for the hard timeout, stacks and
        # all.

        wait_for(lambda: "worker.loop_stalled" in server.log(), timeout=30)
        wait_for(lambda: "worker.timeout_killed" in server.log(), timeout=30)
        log = server.log()
        assert "past the loop stall timeout of 1 s" in log
        assert "plain_asgi.py" in log, "no stack dump"

        wait_for(lambda: json.loads(server.get("/info").body)["pid"] != stalled_pid, timeout=30)
        assert server.get("/").status == 200
    finally:
        server.stop()


def test_recycling_under_the_daemon_moves_traffic_with_no_request_failed(
    tmp_path: Path,
) -> None:
    first = start_daemon(tmp_path)
    second: Daemon | None = None

    try:
        assert (
            first.deploy("shop", SHOP, "--processes", "1", "--max-requests", "50")
            == "muru: release shop@r1 is ready and serving"
        )
        pid = json.loads(first.get("/info").body)["pid"]

        with Load(first.port) as load:
            wait_for(lambda: first.events().count("worker.recycled") >= 3)
            time.sleep(0.3)

        load.check("recycling under the daemon")
        assert json.loads(first.get("/info").body)["pid"] != pid
        assert first.releases("shop") == [
            {"release": "shop@r1", "state": "ready", "serving": "yes"}
        ]
        recycled = [
            line
            for line in first.muructl("events").stdout.splitlines()
            if "worker.recycled" in line
        ]
        assert "max_requests 50" in recycled[0]

        # A daemon that restarts reconnects to the host, so recycling
        # keeps moving traffic after a re-adoption too.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        before = second.events().count("worker.recycled")

        with Load(second.port) as load:
            wait_for(lambda: second.events().count("worker.recycled") >= before + 2)
            time.sleep(0.3)

        load.check("recycling after a daemon restart")
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        second.stop(shutdown=True)
