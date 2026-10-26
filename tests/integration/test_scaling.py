"""The autoscaler (design specification sections 7.2 to 7.4).

A worker group with a scaling policy grows when the edge reports its slots
busy and requests waiting, and shrinks slowly once they are idle, within
its bounds; a pin overrides the autoscaler and outlives a daemon restart;
advisory mode reports without acting; and the policy is validated when a
release is declared.
"""

from __future__ import annotations

import re
import sys
import threading
import time
from pathlib import Path

from support.daemon import SHOP, Daemon, start_daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def scaled_release(tmp_path: Path, scaling: str, processes: int = 1) -> Path:
    """A declared release of the shop with one thread per worker and this policy."""

    config = tmp_path / "shop.toml"
    config.write_text(
        f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
processes = {processes}
threads = 1

[app.shop.release.r1.scaling]
{scaling}
"""
    )
    return config


def status_row(daemon: Daemon, group: str = "default") -> list[str]:
    lines = daemon.muructl("scale", "status", "shop").stdout.splitlines()
    rows = [line.split() for line in lines[2:] if line.startswith(group)]
    assert rows, lines
    return rows[0]


def event_lines(daemon: Daemon) -> list[str]:
    """The event log as muructl prints it, one line per event with its message."""

    return daemon.muructl("events").stdout.splitlines()


def ready_workers(daemon: Daemon) -> int:
    row = status_row(daemon)
    return int(row[1].rstrip("+"))


def wait_for_workers(daemon: Daemon, predicate, timeout: float = 30) -> int:
    deadline = time.monotonic() + timeout
    count = ready_workers(daemon)

    while not predicate(count) and time.monotonic() < deadline:
        time.sleep(0.5)
        count = ready_workers(daemon)

    return count


class Load:
    """Threads that keep slow requests in flight until stopped."""

    def __init__(self, daemon: Daemon, concurrency: int, held_ms: int = 400) -> None:
        self.daemon = daemon
        self.held_ms = held_ms
        self.stopping = threading.Event()
        self.threads = [threading.Thread(target=self.run, daemon=True) for _ in range(concurrency)]

    def __enter__(self) -> Load:
        for thread in self.threads:
            thread.start()

        return self

    def __exit__(self, *_: object) -> None:
        self.stopping.set()

        for thread in self.threads:
            thread.join(timeout=10)

    def run(self) -> None:
        while not self.stopping.is_set():
            try:
                self.daemon.get(f"/slow?ms={self.held_ms}")
            except OSError:
                time.sleep(0.1)


def test_spare_mode_scales_out_under_load_and_back_in_when_idle(
    daemon: Daemon, tmp_path: Path
) -> None:
    config = scaled_release(
        tmp_path,
        """mode = 'spare'
min_workers = 1
max_workers = 4
min_spare = 1
max_spare = 3
scale_out_window = '1s'
scale_in_window = '3s'
hold_after_growth = '2s'
reap_interval = '1s'
idle_reap_after = '3s'
min_lifetime = '1s'
""",
    )
    assert daemon.muructl("apply", "-f", str(config)).returncode == 0
    assert ready_workers(daemon) == 1
    row = status_row(daemon)
    assert row[-1:] == ["idle"] and "spare" in " ".join(row), row

    # Three requests in flight against one single-threaded worker: the
    # group is short and grows, never past its ceiling.

    with Load(daemon, concurrency=3):
        grown = wait_for_workers(daemon, lambda count: count >= 3)
        assert grown >= 3, daemon.muructl("scale", "status", "shop").stdout + daemon.log()[-3000:]
        time.sleep(3)
        assert ready_workers(daemon) <= 4

    # Idle again: one worker drained per interval, down to the floor.

    shrunk = wait_for_workers(daemon, lambda count: count == 1, timeout=60)
    assert shrunk == 1, daemon.muructl("scale", "status", "shop").stdout
    events = [line for line in event_lines(daemon) if "scaling.decision" in line]
    assert any("scale out from 1" in line for line in events), events
    assert any("scale in from" in line for line in events), events
    assert all("advisory" not in line for line in events), events

    # Section 7.3: a decision says what holds the group back, here
    # threads busy sleeping rather than computing, and the status shows
    # the workers' own signals.

    scaled_out = [line for line in events if "scale out from 1" in line]
    assert "diagnosis: I/O bound with exhausted slots" in scaled_out[0], scaled_out
    shown = daemon.muructl("scale", "status", "shop").stdout
    assert re.search(r"workers: cpu \d+%", shown), shown
    assert "diagnosis: " in shown, shown

    # The metrics carry the autoscaler's gauges and counters.

    metrics = daemon.muructl("metrics").stdout
    assert 'muru_workers_desired{app="shop",release="r1",group="default"} 1' in metrics, metrics
    assert (
        'muru_scaling_decisions_total{app="shop",release="r1",group="default",direction="in"}'
        in metrics
    )
    assert "muru_churn_reaps_total" in metrics
    assert "muru_worker_cpu_seconds_total" in metrics


def test_a_pin_overrides_the_autoscaler_and_survives_a_restart(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)

    try:
        config = scaled_release(
            tmp_path,
            """mode = 'spare'
max_workers = 4
scale_in_window = '2s'
idle_reap_after = '2s'
hold_after_growth = '1s'
reap_interval = '1s'
min_lifetime = '1s'
""",
        )
        assert daemon.muructl("apply", "-f", str(config)).returncode == 0
        assert daemon.muructl("scale", "shop", "--processes", "3").stdout.strip() == (
            "muru: shop@r1 default pinned to 3 workers"
        )
        assert wait_for_workers(daemon, lambda count: count == 3) == 3
        assert status_row(daemon)[1:3] == ["3", "pinned"]

        # Idle for longer than every scale-in window, and still pinned.

        time.sleep(5)
        assert ready_workers(daemon) == 3

        over = daemon.muructl("scale", "shop", "--processes", "9", check=False)
        assert over.returncode != 0 and "max_workers" in over.stderr, over.stderr

        # A new daemon over the same runtime directory keeps the pin.

        daemon.stop(shutdown=False)
        daemon = start_daemon(tmp_path, run_dir=daemon.run_dir, port=daemon.port)
        assert status_row(daemon)[1:3] == ["3", "pinned"]
        assert ready_workers(daemon) == 3

        assert daemon.muructl("scale", "shop", "--unpin").stdout.strip() == (
            "muru: shop@r1 default unpinned; the autoscaler decides again"
        )
        assert wait_for_workers(daemon, lambda count: count == 1, timeout=60) == 1
        again = daemon.muructl("scale", "shop", "--unpin", check=False)
        assert again.returncode != 0 and "not pinned" in again.stderr, again.stderr
    finally:
        daemon.stop()


def test_advisory_mode_reports_without_acting(daemon: Daemon, tmp_path: Path) -> None:
    config = scaled_release(
        tmp_path,
        """mode = 'spare'
max_workers = 4
scale_out_window = '1s'
advisory = true
""",
    )
    assert daemon.muructl("apply", "-f", str(config)).returncode == 0

    with Load(daemon, concurrency=3):
        deadline = time.monotonic() + 20

        while time.monotonic() < deadline:
            advice = [line for line in event_lines(daemon) if "advisory: would scale out" in line]

            if advice:
                break

            time.sleep(0.5)

    assert advice, event_lines(daemon)
    assert ready_workers(daemon) == 1
    assert "advisory" in daemon.muructl("scale", "status", "shop").stdout


def test_a_policy_is_validated_when_declared(daemon: Daemon, tmp_path: Path) -> None:
    def refused(scaling: str, processes: int = 1) -> str:
        config = scaled_release(tmp_path, scaling, processes)
        completed = daemon.muructl("apply", "-f", str(config), check=False)
        assert completed.returncode != 0, completed.stdout
        return completed.stdout + completed.stderr

    assert "needs max_workers" in refused("mode = 'spare'")
    assert "scale to zero" in refused("mode = 'spare'\nmax_workers = 4\nmin_workers = 0")
    assert "max_spare must be at least min_spare + 2" in refused(
        "mode = 'spare'\nmax_workers = 8\nmin_spare = 2\nmax_spare = 3"
    )
    assert "takes no other keys" in refused("max_workers = 4")
    assert "slot_utilization" in refused("mode = 'target'\nmax_workers = 4\nslot_utilization = 1.5")
    assert "over max_workers" in refused("mode = 'target'\nmax_workers = 2", processes=3)

    # The resolved policy round-trips through export.

    config = scaled_release(
        tmp_path,
        "mode = 'target'\nmax_workers = 6\nslot_utilization = 0.8\nqueue_wait_p95 = '50ms'",
    )
    assert daemon.muructl("apply", "-f", str(config)).returncode == 0
    exported = daemon.muructl("config", "export").stdout
    assert "[app.shop.release.r1.scaling]" in exported, exported
    assert 'mode = "target"' in exported and "slot_utilization = 0.8" in exported, exported
    assert 'queue_wait_p95 = "50ms"' in exported, exported
    plan = daemon.muructl("apply", "-f", str(config), "--dry-run").stdout
    assert "no changes" in plan, plan
