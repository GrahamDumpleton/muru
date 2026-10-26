"""The node budget manager (design specification sections 7.4, 7.5 and
13.6): the `[daemon.budget]` table caps the workers and memory the
releases hold between them, scale-ups are admitted against it with an
event for what it holds back, a release whose memory does not fit is not
started, and schedules are checked against it when applied and ahead of
their windows.
"""

from __future__ import annotations

import sys
import threading
import time
from datetime import UTC, datetime, timedelta
from pathlib import Path

from support.daemon import SHOP, Daemon, start_daemon


def shop(tmp_path: Path, extra: str = "", releases: tuple[str, ...] = ("r1",)) -> Path:
    """The shop with one thread per worker and a quick spare policy."""

    config = tmp_path / "shop.toml"
    tables = "".join(
        f"""[app.shop.release.{release}]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1
threads = 1

[app.shop.release.{release}.scaling]
mode = 'spare'
min_workers = 1
max_workers = 4
scale_out_window = '1s'
hold_after_growth = '1s'

"""
        for release in releases
    )
    config.write_text(tables + extra)
    return config


def at(seconds: float) -> str:
    """A moment `seconds` from now, as an RFC 3339 time."""

    return (datetime.now(UTC) + timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%SZ")


def ready_workers(daemon: Daemon) -> int:
    lines = daemon.muructl("scale", "status", "shop").stdout.splitlines()
    row = next(line.split() for line in lines[2:] if line.startswith("default"))
    return int(row[1].rstrip("+"))


def wait_for_event(daemon: Daemon, kind: str, timeout: float = 30) -> list[str]:
    deadline = time.monotonic() + timeout
    found: list[str] = []

    while not found and time.monotonic() < deadline:
        found = [line for line in daemon.muructl("events").stdout.splitlines() if kind in line]

        if not found:
            time.sleep(0.5)

    return found


def budget_line(daemon: Daemon, expected: str | None = None, timeout: float = 20) -> str:
    """What `daemon status` says the releases hold of the budget, once it
    says `expected` when given: the figures are taken once a second."""

    deadline = time.monotonic() + timeout

    while True:
        lines = daemon.muructl("daemon", "status").stdout.splitlines()
        line = next((line for line in lines if line.startswith("budget")), "")

        if expected is None or expected in line or time.monotonic() >= deadline:
            return line

        time.sleep(0.5)


def test_scale_ups_are_admitted_within_the_worker_budget(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra="[daemon.budget]\nworkers = 2\n")
    stopping = threading.Event()

    def load() -> None:
        while not stopping.is_set():
            try:
                daemon.get("/slow?ms=400")
            except OSError:
                time.sleep(0.1)

    threads = [threading.Thread(target=load, daemon=True) for _ in range(4)]

    try:
        config = shop(tmp_path, "[app.shop]\npriority = 5\n")
        applied = daemon.muructl("apply", "-f", str(config)).stdout
        assert "set-priority" in applied, applied
        assert "[priority 5]" in daemon.muructl("app", "list").stdout
        assert budget_line(daemon, "1 of 2") == "budget   1 of 2 workers"

        # Four requests in flight against one single-threaded worker: the
        # group asks for more than the node's two workers, and is given
        # what fits.

        for thread in threads:
            thread.start()

        denied = wait_for_event(daemon, "scaling.denied_by_budget")
        assert denied, daemon.muructl("scale", "status", "shop").stdout
        assert "1 of the 3 more workers asked for fit the node's worker budget" in denied[0]
        assert "at priority 5" in denied[0]

        time.sleep(4)
        assert ready_workers(daemon) == 2
        assert budget_line(daemon, "2 of 2") == "budget   2 of 2 workers"
        assert "held at 2" in daemon.muructl("scale", "status", "shop").stdout

        # A floor beyond the budget is reported when applied, and once
        # ahead of its window.

        schedule = f"""[app.shop]
priority = 5

[[app.shop.schedule]]
name = 'busy'
start = '{at(120)}'
end = '{at(600)}'
min_workers = 4
"""
        applied = daemon.muructl("apply", "-f", str(shop(tmp_path, schedule))).stdout
        assert "warning: schedule shop.busy: a floor of 4 workers for shop" in applied, applied
        assert "the node's worker budget has room for 0" in applied, applied

        ahead = wait_for_event(daemon, "schedule.over_budget")
        assert ahead and "a floor of 4 workers for shop (default group)" in ahead[0], ahead
    finally:
        stopping.set()

        for thread in threads:
            if thread.is_alive():
                thread.join(timeout=10)

        daemon.stop()


def test_a_release_whose_memory_does_not_fit_is_not_started(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra="[daemon.budget]\nmemory = '1MB'\n")

    try:
        # Nothing is known of the shop's memory before it runs, so the
        # first release starts.

        assert daemon.muructl("apply", "-f", str(shop(tmp_path))).returncode == 0

        deadline = time.monotonic() + 20

        while budget_line(daemon).startswith("budget   0B") and time.monotonic() < deadline:
            time.sleep(0.5)

        held = budget_line(daemon)
        assert held.endswith("of 1.0MB memory") and not held.startswith("budget   0B"), held

        # Measured, its next release is over the budget: apply warns, and
        # the deploy is refused without a record.

        planned = daemon.muructl(
            "apply", "-f", str(shop(tmp_path, releases=("r1", "r2"))), check=False
        )
        assert planned.returncode == 1, planned.stdout + planned.stderr
        assert "warning: shop@r2: the release needs about" in planned.stdout, planned.stdout
        assert "so its deploy would be refused" in planned.stdout
        assert "not started: shop: the release needs about" in planned.stdout

        releases = daemon.muructl("release", "list", "shop").stdout
        assert "r2" not in releases, releases
        assert wait_for_event(daemon, "scaling.denied_by_budget")
    finally:
        daemon.stop()
