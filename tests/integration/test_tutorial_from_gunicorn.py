"""The steps of the gunicorn tutorial, with the tutorial's own application.

docs/tutorials/from-gunicorn.md includes docs/tutorials/code/legacy/legacy.py,
and this test runs it through the page's cutover, so a change that breaks
a step fails here rather than for a reader. gunicorn runs as
`python -m gunicorn` from the test's interpreter, where the page runs the
project's `.venv/bin/gunicorn`, and the canary's steps hold for 2 s
rather than 30 s.
"""

from __future__ import annotations

import sys
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon
from support.load import Load

LEGACY = support.ROOT / "docs" / "tutorials" / "code" / "legacy"


def answer(daemon: Daemon, **headers: str) -> str:
    return daemon.request("/", headers=headers).body.decode()


def test_the_gunicorn_tutorial(tmp_path: Path) -> None:
    try:
        __import__("gunicorn")
    except ImportError:
        support.unavailable("gunicorn is not installed")

    daemon = start_daemon(tmp_path)

    try:
        # Hand gunicorn to the daemon.

        out = daemon.muructl(
            "deploy",
            "legacy",
            str(LEGACY),
            "--server",
            "gunicorn",
            "--",
            sys.executable,
            "-m",
            "gunicorn",
            "--workers",
            "2",
            "--bind",
            "{bind}",
            "legacy:app",
        ).stdout.strip()
        assert out == "muru: release legacy@r1 is ready and serving"
        assert answer(daemon).startswith("Hello from gunicorn/")
        assert "1 gunicorn" in daemon.muructl("release", "list", "legacy").stdout

        # The same application on Muru's workers, held.

        out = daemon.muructl(
            "deploy",
            "legacy",
            str(LEGACY),
            "--entrypoint",
            "legacy:app",
            "--python",
            sys.executable,
            "--hold",
        ).stdout.strip()
        assert out == "muru: release legacy@r2 is ready and held: not serving"

        # Cut over with requests running, previewing both releases on the
        # way, then retire gunicorn: no request fails throughout.

        with Load(daemon.port, clients=4) as load:
            daemon.muructl(
                "rollout",
                "canary",
                "legacy@r2",
                "--steps",
                "10,50",
                "--hold",
                "2s",
                "--min-requests",
                "20",
            )
            assert answer(daemon, **{"Muru-Preview": "r2"}) == "Hello from Muru\n"
            assert answer(daemon, **{"Muru-Preview": "r1"}).startswith("Hello from gunicorn/")
            followed = daemon.muructl("rollout", "status", "legacy", "--wait").stdout
            assert "rollout 1 promoted: every step passed its gates" in followed
            assert answer(daemon) == "Hello from Muru\n"
            out = daemon.muructl("drain", "legacy@r1").stdout.strip()
            assert out == "muru: release legacy@r1 is stopped"

        load.check("the cutover from gunicorn to Muru's workers", at_least=100)
        assert "Shutting down: Master" in daemon.muructl("logs", "legacy@r1").stdout
    finally:
        daemon.stop()
