"""The GIL stall timeout (design specification section 7.6).

A thread in every worker takes the interpreter lock every half second, and
a watchdog outside Python replaces the worker, as for the hard timeout,
when no thread has managed to for the GIL stall timeout: a C call that
never returns with the lock held, or a deadlock. The host replaces a
worker whose reports have stopped for the same period, such as one
stopped by a signal.
"""

from __future__ import annotations

import json
import os
import signal
import sys
import threading
import time
from collections.abc import Callable

import pytest
from support import server as support


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def pid_serving(server: support.Server) -> int | None:
    """The pid that answers, or None while no worker does."""

    response = server.get("/info")

    if response.status != 200:
        return None

    return int(json.loads(response.body)["pid"])


def start(gil_stall: str) -> support.Server:
    return support.start(
        "plain_wsgi:application",
        workers=1,
        threads=2,
        args=[
            "--timeout",
            f"gil_stall={gil_stall}",
            "--timeout",
            "hard_drain_grace=1s",
            "--timeout",
            "shutdown=1s",
        ],
    )


@pytest.mark.skipif(
    not getattr(sys, "_is_gil_enabled", lambda: True)(),
    reason="a free-threaded build has no interpreter lock to hold",
)
def test_a_worker_holding_the_gil_is_replaced() -> None:
    server = start("1s")

    try:
        stuck = pid_serving(server)
        assert stuck is not None
        thread = threading.Thread(target=lambda: server.get("/hold?seconds=60"), daemon=True)
        thread.start()

        # Nothing in the worker runs Python while the call holds the lock,
        # so the native watchdog tells the host, which replaces the worker
        # as for the hard timeout, with its stacks in the log.

        wait_for(lambda: "worker.gil_stalled" in server.log())
        wait_for(lambda: "worker.timeout_killed" in server.log())
        log = server.log()
        assert "past the GIL stall timeout of 1 s" in log, log
        assert "no thread of it ran Python for the GIL stall timeout of 1 s" in log, log
        assert "hold_the_interpreter" in log, "no stack dump"
        wait_for(lambda: pid_serving(server) not in (None, stuck))
        assert server.get("/sleep?seconds=0.1").status == 200
    finally:
        server.stop()


def test_a_worker_that_stops_reporting_is_replaced() -> None:
    server = start("2s")

    try:
        stopped = pid_serving(server)
        assert stopped is not None
        os.kill(stopped, signal.SIGSTOP)

        # A stopped process sends no report, from Python or outside it,
        # so the host notices the silence and replaces the worker.

        wait_for(lambda: "worker.timeout_killed" in server.log())
        log = server.log()
        assert "sent no report for" in log and "GIL stall timeout of 2 s" in log, log
        wait_for(lambda: pid_serving(server) not in (None, stopped))
        assert server.get("/").status == 200
    finally:
        try:
            os.kill(stopped, signal.SIGCONT)
        except ProcessLookupError:
            pass

        server.stop()
