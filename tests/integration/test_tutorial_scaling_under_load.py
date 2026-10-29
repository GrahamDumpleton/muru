"""The steps of the scaling tutorial, with the tutorial's own files.

docs/tutorials/scaling-under-load.md includes the files under
docs/tutorials/code/busy, and this test runs them through the page's
steps, so a change that breaks one fails here rather than for a reader.
The policy's windows are shortened further than the page's, to keep the
test short; the decisions are the same.
"""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon

BUSY = support.ROOT / "docs" / "tutorials" / "code" / "busy"

SHORTER = {
    'scale_in_window = "20s"': 'scale_in_window = "4s"',
    'hold_after_growth = "20s"': 'hold_after_growth = "4s"',
    'reap_interval = "5s"': 'reap_interval = "1s"',
    'min_lifetime = "10s"': 'min_lifetime = "2s"',
    'idle_reap_after = "30s"': 'idle_reap_after = "6s"',
}


def workers(daemon: Daemon) -> str:
    """The WORKERS column of the default group in `scale status`."""

    for line in daemon.muructl("scale", "status", "busy").stdout.splitlines():
        if line.startswith("default"):
            return line[len("default") :].strip().split("  ")[0].strip()

    raise AssertionError("no default group in scale status")


def wait_for(condition: Callable[[], bool], timeout: float = 60) -> None:
    deadline = time.monotonic() + timeout

    while not condition():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.5)


def test_the_scaling_tutorial(tmp_path: Path) -> None:
    project = tmp_path / "busy"
    shutil.copytree(BUSY, project)
    pyproject = project / "pyproject.toml"
    text = pyproject.read_text()

    for before, after in SHORTER.items():
        assert before in text
        text = text.replace(before, after)

    pyproject.write_text(text)
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")

    try:
        assert daemon.deploy("busy", project) == "muru: release busy@r1 is ready and serving"
        status = daemon.muructl("scale", "status", "busy").stdout
        assert "spare 1-8 workers, 1-3 idle" in status
        assert workers(daemon) == "1"

        # Eight clients of load: the group grows to five workers, four to
        # carry it and one in hand, and nothing fails.

        url = f"http://127.0.0.1:{daemon.port}/"
        load = subprocess.run(
            [sys.executable, str(project / "load.py"), "8", "15", url],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        assert load.splitlines()[-1].endswith("requests served, 0 failed"), load

        # On a slow machine the clients can arrive over more than one
        # decision, so the group may grow in steps; it ends at five. The
        # diagnosis is read from the decision, made while slots ran out:
        # at five workers eight clients fill eight of ten slots at most,
        # on the threshold, so the status's diagnosis can go either way.

        events = daemon.muructl("events").stdout
        scale_out = (
            r"default: scale out from \d+ to 5 workers: "
            r".*diagnosis: I/O bound with exhausted slots"
        )
        assert re.search(scale_out, events), events
        status = daemon.muructl("scale", "status", "busy").stdout
        assert "last: scaled out to 5: short by" in status, status

        # Without load it comes back to one worker, by both rules.

        wait_for(lambda: workers(daemon) == "1")
        events = daemon.muructl("events").stdout
        assert "over max_spare 3" in events, events
        assert "default: scale in from 2 to 1 workers: no request for 6s" in events, events

        # Scale to zero, and wake on the next request.

        pyproject.write_text(pyproject.read_text().replace("min_workers = 1", "min_workers = 0"))
        assert daemon.deploy("busy", project) == "muru: release busy@r2 is ready and serving"
        wait_for(lambda: workers(daemon) == "0")
        response = daemon.get()
        assert (response.status, response.body) == (200, b"done\n")
        events = daemon.muructl("events").stdout
        assert "scale out from 0 to 1 workers: a request is waiting for a group scaled to zero" in (
            events
        )

        # Pin it by hand, and hand it back.

        pinned = daemon.muructl("scale", "busy", "--processes", "3").stdout.strip()
        assert pinned == "muru: busy@r2 default pinned to 3 workers"
        wait_for(lambda: workers(daemon) == "3 pinned")
        unpinned = daemon.muructl("scale", "busy", "--unpin").stdout.strip()
        assert unpinned == "muru: busy@r2 default unpinned; the autoscaler decides again"
    finally:
        daemon.stop()
