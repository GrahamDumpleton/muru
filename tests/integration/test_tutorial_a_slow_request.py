"""The steps of the slow request tutorial, with the tutorial's own files.

docs/tutorials/a-slow-request.md includes the files under
docs/tutorials/code/reports, and this test runs them through the page's
steps, so a change that breaks one fails here rather than for a reader.
The profile runs for 3 s rather than the page's 10 s.
"""

from __future__ import annotations

import json
import threading
import time
from pathlib import Path

from support import server as support
from support.daemon import start_daemon

REPORTS = support.ROOT / "docs" / "tutorials" / "code" / "reports"


def test_the_slow_request_tutorial(tmp_path: Path) -> None:
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")

    try:
        assert daemon.deploy("reports", REPORTS) == "muru: release reports@r1 is ready and serving"
        assert daemon.get("/report?months=1").body == b"1 months: 15999998\n"

        # The worst case is stopped by the soft timeout, and the event
        # and the log say where it was.

        stopped = daemon.request("/report?months=12", headers={"Accept": "application/json"})
        assert stopped.status == 504
        assert json.loads(stopped.body)["reason"] == "soft_timeout"
        events = daemon.muructl("events").stdout
        assert "ran past the soft timeout of 3 s at report.py line 14 in summarise" in events
        assert "muru.RequestTimeout" in daemon.muructl("logs", "reports").stdout

        # A request in progress, in the stacks.

        background = threading.Thread(target=daemon.get, args=("/report?months=6",))
        background.start()

        try:
            time.sleep(1)
            stacks = daemon.muructl("debug", "stacks", "reports").stdout
        finally:
            background.join()

        assert "serving GET /report?months=6 for" in stacks
        assert "in build_report" in stacks

        # A profile under load names where the time goes.

        stop = threading.Event()

        def loop() -> None:
            while not stop.is_set():
                daemon.get("/report?months=2")

        runner = threading.Thread(target=loop)
        runner.start()
        folded = tmp_path / "report.folded"

        try:
            profile = daemon.muructl(
                "debug", "profile", "reports", "--for", "3s", "--output", str(folded)
            )
        finally:
            stop.set()
            runner.join()

        assert "summarise (report.py:10)" in profile.stderr
        assert "fetch_rows (report.py:5)" in profile.stderr
        assert "build_report (report.py:19);summarise (report.py:10)" in folded.read_text()
    finally:
        daemon.stop()
