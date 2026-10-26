"""Scale to zero and warm retention (design specification sections 7.4
and 9.1): a group with no floor drains its last worker once idle and the
next request wakes it, a release with a host_idle_timeout stops its host
too and the next request starts it again, and a superseded release keeps
one worker per group while it stays warm.
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

from support.daemon import SHOP, Daemon, start_daemon

LIMITS = "[limits]\nqueue_timeout = '30s'\n"


def shop(tmp_path: Path, scaling: str, releases: tuple[str, ...] = ("r1",)) -> Path:
    """The shop with one thread per worker and this policy on each release."""

    config = tmp_path / "shop.toml"
    config.write_text(
        "".join(
            f"""[app.shop.release.{release}]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1
threads = 1

[app.shop.release.{release}.scaling]
{scaling}
"""
            for release in releases
        )
    )
    return config


IDLE_QUICKLY = """mode = 'spare'
min_workers = 0
max_workers = 2
idle_reap_after = '2s'
scale_in_window = '2s'
hold_after_growth = '1s'
reap_interval = '1s'
min_lifetime = '1s'
"""


def workers(daemon: Daemon, target: str = "shop") -> int:
    lines = daemon.muructl("scale", "status", target).stdout.splitlines()
    row = next(line.split() for line in lines[2:] if line.startswith("default"))
    return int(row[1].rstrip("+"))


def wait_until(predicate, timeout: float = 30) -> bool:
    deadline = time.monotonic() + timeout

    while not predicate():
        if time.monotonic() >= deadline:
            return False

        time.sleep(0.5)

    return True


def events(daemon: Daemon, kind: str) -> list[str]:
    return [line for line in daemon.muructl("events").stdout.splitlines() if kind in line]


def test_an_idle_group_scales_to_zero_and_a_request_wakes_it(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra=LIMITS)

    try:
        assert daemon.muructl("apply", "-f", str(shop(tmp_path, IDLE_QUICKLY))).returncode == 0
        assert daemon.get("/").status == 200
        assert wait_until(lambda: workers(daemon) == 0), daemon.muructl(
            "scale", "status", "shop"
        ).stdout

        # The request waits at the edge while a worker forks, and is
        # served.

        started = time.monotonic()
        response = daemon.get("/")
        waited = time.monotonic() - started
        assert response.status == 200, response
        assert waited < 10, waited
        assert wait_until(lambda: workers(daemon) == 1, timeout=5)

        woken = [line for line in events(daemon, "scaling.decision") if "from 0 to 1" in line]
        assert woken and "a request is waiting for a group scaled to zero" in woken[0], woken
    finally:
        daemon.stop()


def test_an_idle_host_stops_and_the_next_request_starts_it(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra=LIMITS)

    try:
        config = shop(tmp_path, IDLE_QUICKLY + "host_idle_timeout = '2s'\n")
        assert daemon.muructl("apply", "-f", str(config)).returncode == 0
        assert wait_until(lambda: bool(events(daemon, "release.idle")), timeout=40), daemon.muructl(
            "events"
        ).stdout
        assert "its host stopped" in events(daemon, "release.idle")[0]
        assert "idle: its host has stopped" in daemon.muructl("release", "list", "shop").stdout

        response = daemon.get("/")
        assert response.status == 200, response
        assert events(daemon, "release.woken")
        assert workers(daemon) >= 1
    finally:
        daemon.stop()


def test_a_retained_release_keeps_one_worker_per_group(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)

    try:
        config = tmp_path / "shop.toml"
        config.write_text(
            f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
processes = 3
threads = 1
"""
        )
        assert daemon.muructl("apply", "-f", str(config)).returncode == 0
        assert workers(daemon, "shop@r1") == 3

        deployed = daemon.muructl("deploy", "shop", str(SHOP), "--python", sys.executable)
        assert deployed.returncode == 0, deployed.stderr
        assert wait_until(lambda: workers(daemon, "shop@r1") == 1), daemon.muructl(
            "scale", "status", "shop@r1"
        ).stdout
        retained = events(daemon, "release.retained")
        assert retained and "with 1 worker per group" in retained[0], retained

        # A rollback serves at once from the warm worker, and the group
        # grows back to its count.

        assert daemon.muructl("rollback", "shop").returncode == 0
        assert daemon.get("/").status == 200
        assert wait_until(lambda: workers(daemon, "shop@r1") == 3)
    finally:
        daemon.stop()
