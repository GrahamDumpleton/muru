"""Garbage collection measured in each worker (design specification
section 11.1): every collection is timed with its generation, what it
freed and whether requests were in flight, and the figures reach the
metrics, muructl debug memory and the scaling signals."""

from __future__ import annotations

import re
import sys
import threading
import time
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError("the condition was not met in time")

        time.sleep(0.2)


@pytest.fixture
def daemon(tmp_path: Path) -> Iterator[Daemon]:
    """A daemon that allows debug exec, so a collection can be forced."""

    instance = start_daemon(tmp_path, config_extra="[daemon]\ndebug_exec = true\n")

    try:
        yield instance
    finally:
        instance.stop()


def generation() -> str:
    """The label a full collection carries on this interpreter."""

    import sysconfig

    return "full" if sysconfig.get_config_var("Py_GIL_DISABLED") == 1 else "2"


def test_collections_are_counted_with_the_requests_they_paused(daemon: Daemon) -> None:
    assert daemon.deploy(
        "plain",
        support.FIXTURES,
        "--entrypoint",
        "plain_wsgi:application",
        "--processes",
        "1",
        "--threads",
        "2",
        "--memory",
        "check_interval=1s",
    ) == ("muru: release plain@r1 is ready and serving")

    # A full collection with nothing in flight, then one while a request
    # sleeps in the worker.

    collect = ["debug", "exec", "plain", "--", "import gc; gc.collect()"]
    assert daemon.muructl(*collect).returncode == 0

    sleeper = threading.Thread(target=lambda: daemon.get("/sleep?seconds=2"))
    sleeper.start()

    try:
        time.sleep(0.5)
        assert daemon.muructl(*collect).returncode == 0
    finally:
        sleeper.join()

    # The worker's own view, by generation.

    shown = daemon.muructl("debug", "memory", "plain").stdout
    line = next(line for line in shown.splitlines() if f"gc generation {generation()}:" in line)

    match = re.search(
        r"(\d+) collections, ([\d.]+) ms in all, p95 under ([\d.]+ ms|-), (\d+) objects freed, "
        r"(\d+) while (\d+) requests were in flight",
        line,
    )
    assert match, line
    collections, _, _, _, busy, waited = match.groups()
    assert int(collections) >= 2 and int(busy) >= 1 and int(waited) >= 1, line

    # Exported with the worker's next memory report, by generation and
    # by whether requests were in flight.

    labels = f'app="plain",release="r1",group="default",worker="0",generation="{generation()}"'

    def exported() -> str:
        return daemon.muructl("metrics").stdout

    wait_for(lambda: f"muru_gc_collections_total{{{labels}}}" in exported())
    text = exported()
    assert "# TYPE muru_gc_collections_total counter\n" in text
    assert "# TYPE muru_gc_pause_seconds histogram\n" in text
    assert re.search(rf"muru_gc_collections_total\{{{re.escape(labels)}\}} [2-9]\d*\n", text), text
    assert re.search(rf"muru_gc_seconds_total\{{{re.escape(labels)}\}} [0-9.e-]+\n", text), text
    assert re.search(rf"muru_gc_collected_objects_total\{{{re.escape(labels)}\}} \d+\n", text), text

    for requests in ("none", "in_flight"):
        count = rf'muru_gc_pause_seconds_count\{{{re.escape(labels)},requests="{requests}"\}}'
        assert re.search(count + r" [1-9]\d*\n", text), (requests, text)
        assert (
            f'muru_gc_pause_seconds_bucket{{{labels},requests="{requests}",le="+Inf"}}' in text
        ), text

    # The scaling signals carry the share of time collecting.

    assert daemon.muructl("scale", "status", "plain").returncode == 0
    wait_for(
        lambda: (
            re.search(
                r"workers: cpu \d+%, gc \d+%", daemon.muructl("scale", "status", "plain").stdout
            )
            is not None
        ),
        timeout=20,
    )


def test_the_callback_costs_the_worker_little() -> None:
    """The callback is on in every worker with no setting, so it is held
    to a few microseconds a collection: a start and a stop call, against
    an automatic collection that scans at least a threshold's worth of
    new objects and takes tens of microseconds or more."""

    import gc

    from muru import _native

    gc.collect()
    info = {"generation": 0, "collected": 0, "uncollectable": 0}
    rounds = 20_000
    started = time.perf_counter()

    for _ in range(rounds):
        _native.gc_callback("start", info)
        _native.gc_callback("stop", info)

    per_collection_us = (time.perf_counter() - started) / rounds * 1e6
    assert per_collection_us < 10, per_collection_us

    # A youngest-generation collection over the threshold's worth of
    # fresh cyclic objects, for the comparison, with the automatic
    # collections off so each round collects once, when asked.

    threshold = max(gc.get_threshold()[0], 1)
    rounds //= 100
    spent = 0.0
    gc.disable()

    try:
        for _ in range(rounds):
            cycles: list[list[object]] = [[] for _ in range(threshold)]

            for cycle in cycles:
                cycle.append(cycle)

            del cycles
            started = time.perf_counter()
            gc.collect(0)
            spent += time.perf_counter() - started
    finally:
        gc.enable()

    collection_us = spent / rounds * 1e6
    print(f"callback {per_collection_us:.2f} us a collection, collection {collection_us:.1f} us")
    assert per_collection_us * 10 < collection_us, (per_collection_us, collection_us)


def test_the_callback_reports_what_it_timed() -> None:
    import gc

    from muru import _native

    before = {entry["generation"]: entry["collections"] for entry in _native.gc_stats()}
    gc.callbacks.append(_native.gc_callback)

    try:
        gc.collect()
    finally:
        gc.callbacks.remove(_native.gc_callback)

    after = {entry["generation"]: entry for entry in _native.gc_stats()}
    entry = after[generation()]
    assert entry["collections"] == before.get(generation(), 0) + 1
    assert entry["ns"] > 0
    assert entry["busy"] == 0, "nothing is served in a test process"
    assert len(entry["idle_hist"]) == 11 and entry["idle_hist"][-1] >= 1
    assert sys.version_info >= (3, 11)
