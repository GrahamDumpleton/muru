"""Scheduled capacity and deploy freezes (design specification sections
7.5 and 9.6): an application's schedules raise a worker group's floor
and ceiling for their windows and can freeze deploys, `muructl scale
--min` sets a floor until a time, and the schedules survive an export.
"""

from __future__ import annotations

import sys
import time
from collections.abc import Callable
from datetime import UTC, datetime, timedelta
from pathlib import Path

from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture

SPARE = """[app.shop.release.r1.scaling]
mode = 'spare'
min_workers = 1
max_workers = 2
scale_out_window = '1s'
scale_in_window = '2s'
idle_reap_after = '2s'
hold_after_growth = '1s'
reap_interval = '1s'
min_lifetime = '1s'
"""


def at(seconds: float) -> str:
    """A moment `seconds` from now, as an RFC 3339 time."""

    return (datetime.now(UTC) + timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%SZ")


def shop(tmp_path: Path, extra: str = "", releases: tuple[str, ...] = ("r1",)) -> Path:
    """The shop with one thread per worker, a spare policy on r1, and `extra`."""

    config = tmp_path / "shop.toml"
    tables = "".join(
        f"""[app.shop.release.{release}]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1
threads = 1

"""
        for release in releases
    )
    config.write_text(tables + SPARE + extra)
    return config


def workers(daemon: Daemon) -> int:
    lines = daemon.muructl("scale", "status", "shop").stdout.splitlines()
    row = next(line.split() for line in lines[2:] if line.startswith("default"))
    return int(row[1])


def eventually(check: Callable[[], bool], timeout: float = 30.0) -> bool:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if check():
            return True

        time.sleep(0.5)

    return False


def test_a_schedule_raises_the_floor_for_its_window(daemon: Daemon, tmp_path: Path) -> None:
    surge = f"""
[[app.shop.schedule]]
name = 'surge'
start = '{at(2)}'
end = '{at(12)}'
min_workers = 3
max_workers = 5
"""
    daemon.muructl("apply", "-f", str(shop(tmp_path, surge)))
    assert workers(daemon) == 1
    assert "shop.surge" in daemon.muructl("overlay", "list").stdout

    # The floor is over the policy's max_workers of 2: the ceiling rises
    # with it.

    assert eventually(lambda: workers(daemon) == 3)
    status = daemon.muructl("scale", "status", "shop").stdout
    assert "scheduled: 3-5 workers from shop.surge" in status
    assert "raising toward the floor of 3 that shop.surge sets" in daemon.muructl("events").stdout

    # After the window the idle workers go by the policy's own rules.

    assert eventually(lambda: workers(daemon) == 1, timeout=60)
    assert "scheduled" not in daemon.muructl("scale", "status", "shop").stdout
    kinds = daemon.events()
    assert "schedule.activated" in kinds and "schedule.expired" in kinds


def test_scale_min_is_a_temporary_floor(daemon: Daemon, tmp_path: Path) -> None:
    daemon.muructl("apply", "-f", str(shop(tmp_path)))
    raised = daemon.muructl("scale", "shop", "--min", "2", "--for", "6s")
    assert "muru: shop default kept at 2 workers or more until" in raised.stdout
    assert "shop:scale" in daemon.muructl("overlay", "list").stdout
    assert eventually(lambda: workers(daemon) == 2)

    # It is forgotten once it ends, and apply leaves it alone meanwhile.

    daemon.muructl("apply", "--prune", "-f", str(shop(tmp_path)))
    assert eventually(lambda: "shop:scale" not in daemon.muructl("overlay", "list").stdout)
    assert eventually(lambda: workers(daemon) == 1, timeout=60)

    refused = daemon.muructl("scale", "shop", "--min", "2", check=False)
    assert refused.returncode != 0 and "--min needs --until" in refused.stderr


def test_a_freeze_refuses_deploys_unless_overridden(daemon: Daemon, tmp_path: Path) -> None:
    freeze = f"""
[[app.shop.schedule]]
name = 'quiet'
start = '{at(-60)}'
end = '{at(3600)}'
freeze_deploys = true
"""
    daemon.muructl("apply", "-f", str(shop(tmp_path, freeze)))
    assert eventually(lambda: "active (schedule)" in daemon.muructl("overlay", "list").stdout)

    refused = daemon.muructl("deploy", "shop", str(SHOP), "--python", sys.executable, check=False)
    assert refused.returncode != 0
    assert "deploys of shop are frozen by overlay shop.quiet" in refused.stderr

    second = shop(tmp_path, freeze, releases=("r1", "r2"))
    blocked = daemon.muructl("apply", "-f", str(second), check=False)
    assert blocked.returncode != 0
    assert "shop@r2: deploys of shop are frozen by overlay shop.quiet" in blocked.stdout

    applied = daemon.muructl("apply", "--override-freeze", "-f", str(second))
    assert applied.returncode == 0, applied.stdout
    events = daemon.muructl("events").stdout
    assert "deploy.freeze_overridden" in events
    assert "apply deploys shop@r2 despite the freeze set by overlay shop.quiet" in events


def test_export_writes_schedules_back(daemon: Daemon, tmp_path: Path) -> None:
    nightly = """
[[app.shop.schedule]]
name = 'nightly'
cron = '0 2 * * *'
duration = '1h'
timezone = 'Australia/Sydney'
min_workers = 2
warm_up = '15m'
scale_in = 'hold'
"""
    config = shop(tmp_path, nightly)
    daemon.muructl("apply", "-f", str(config))
    exported = daemon.muructl("config", "export").stdout
    assert "[[app.shop.schedule]]" in exported
    assert 'cron = "0 2 * * *"' in exported
    assert 'warm_up = "15m"' in exported
    assert "[overlay" not in exported

    # Applied back with --prune, nothing changes.

    again = tmp_path / "exported.toml"
    again.write_text(exported)
    report = daemon.muructl("apply", "--prune", "--dry-run", "-f", str(again), check=False)
    assert "remove-overlay" not in report.stdout, report.stdout
