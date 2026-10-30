"""Restarting a worker by hand (design specification section 7.6).

`muructl restart` replaces one worker of a release through the path a
recycle takes, so no request is lost, or every worker in turn with --all.
With --force the worker leaves routing at once, its stacks are written to
the log and it is killed without being asked to stop, which also recovers
a worker that is frozen.
"""

from __future__ import annotations

import json
import os
import signal
import time
from collections.abc import Callable

from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture
from support.load import Load

daemon = daemon_fixture


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def workers(daemon: Daemon) -> dict[int, int]:
    """The pid serving at each worker index, from the host's status."""

    status = json.loads(daemon.muructl("debug", "status", "shop", "--json").stdout)
    return {
        int(worker["index"]): int(worker["pid"])
        for worker in status["workers"]
        if worker["ok"] and worker["result"]["ready"] and not worker["result"]["replaced"]
    }


def deploy_shop(daemon: Daemon) -> dict[int, int]:
    assert daemon.deploy(
        "shop",
        SHOP,
        "--processes",
        "2",
        "--timeout",
        "hard_drain_grace=1s",
        "--timeout",
        "shutdown=2s",
    ) == ("muru: release shop@r1 is ready and serving")
    pids = workers(daemon)
    assert sorted(pids) == [0, 1], pids
    return pids


def test_a_worker_is_replaced_with_no_request_lost(daemon: Daemon) -> None:
    before = deploy_shop(daemon)

    with Load(daemon.port) as load:
        lines = daemon.muructl("restart", "shop", "--worker", "0").stdout.splitlines()
        time.sleep(0.5)

    load.check("a restart")
    after = workers(daemon)
    assert after[0] != before[0] and after[1] == before[1], (before, after)
    assert lines == [
        f"muru: worker 0 (pid {before[0]}) is being replaced by pid {after[0]}",
        f"muru: worker 0 is served by pid {after[0]}; pid {before[0]} has stopped",
    ]
    events = daemon.events()
    assert "worker.restarted" in events, events
    log = daemon.log()
    assert f"worker 0 (pid {before[0]}) replaced by pid {after[0]}: restarted on request" in log


def test_a_worker_is_named_by_pid_and_a_missing_one_is_refused(daemon: Daemon) -> None:
    before = deploy_shop(daemon)
    daemon.muructl("restart", "shop@r1", "--pid", str(before[1]))
    after = workers(daemon)
    assert after[1] != before[1] and after[0] == before[0], (before, after)

    refused = daemon.muructl("restart", "shop", "--worker", "7", check=False)
    assert refused.returncode != 0
    assert "shop@r1 has no serving worker with index 7" in refused.stderr, refused.stderr


def test_force_recovers_a_frozen_worker(daemon: Daemon) -> None:
    before = deploy_shop(daemon)
    daemon.wait_for_routing("shop")
    os.kill(before[0], signal.SIGSTOP)

    try:
        started = time.monotonic()
        lines = daemon.muructl("restart", "shop", "--worker", "0", "--force").stdout.splitlines()
        assert time.monotonic() - started < 20
        assert lines[0].endswith(", killed at once"), lines
        after = workers(daemon)
        assert after[0] != before[0], (before, after)
        assert daemon.get("/").status == 200
        log = daemon.log()
        assert "restarted on request, killed at once" in log, log
    finally:
        try:
            os.kill(before[0], signal.SIGCONT)
        except ProcessLookupError:
            pass


def test_all_restarts_every_worker_in_turn(daemon: Daemon) -> None:
    before = deploy_shop(daemon)

    with Load(daemon.port) as load:
        output = daemon.muructl("restart", "shop", "--all").stdout
        time.sleep(0.5)

    load.check("a rolling restart")
    after = workers(daemon)
    assert after[0] != before[0] and after[1] != before[1], (before, after)
    assert output.count("is served by pid") == 2, output
