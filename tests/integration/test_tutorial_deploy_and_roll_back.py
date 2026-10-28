"""The steps of the deploy and roll back tutorial, with the tutorial's own releases.

docs/tutorials/deploy-and-roll-back.md includes the three versions under
docs/tutorials/code/releases, and this test deploys them through the
page's steps, so a change that breaks one fails here rather than for a
reader.
"""

from __future__ import annotations

import threading
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon

RELEASES = support.ROOT / "docs" / "tutorials" / "code" / "releases"


def greeting(daemon: Daemon) -> str:
    """What the application answers."""

    return daemon.get().body.decode()


def test_the_deploy_and_roll_back_tutorial(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)

    try:
        # Start the daemon: with nothing deployed, every request is 503.

        empty = daemon.get()
        assert empty.status == 503
        assert b"no_workers" in empty.body

        # Deploy the first release.

        out = daemon.deploy("hello", RELEASES / "v1", "--entrypoint", "app:application")
        assert out == "muru: release hello@r1 is ready and serving"
        assert greeting(daemon) == "Hello from hello@r1\n"
        assert "traffic.switched" in daemon.events()

        # A release that fails leaves the first one serving, and its log
        # says why.

        failed = daemon.muructl("deploy", "hello", str(RELEASES / "v2"), check=False)
        assert failed.returncode == 1
        assert "before its workers were ready" in failed.stderr
        assert greeting(daemon) == "Hello from hello@r1\n"
        assert "KeyError: 'GREETING'" in daemon.muructl("logs", "hello@r2").stdout
        states = {row["release"]: row["state"] for row in daemon.releases("hello")}
        assert states == {"hello@r1": "ready", "hello@r2": "failed"}

        # Deploy a fix, with requests running: none of them fails.

        statuses: list[int] = []
        stop = threading.Event()

        def loop() -> None:
            while not stop.is_set():
                statuses.append(daemon.get().status)

        runner = threading.Thread(target=loop)
        runner.start()

        try:
            out = daemon.muructl("deploy", "hello", str(RELEASES / "v3")).stdout.strip()
        finally:
            stop.set()
            runner.join()

        assert out == "muru: release hello@r3 is ready and serving"
        assert statuses and set(statuses) == {200}
        assert greeting(daemon) == "Hello again from hello@r3\n"
        states = {row["release"]: row["state"] for row in daemon.releases("hello")}
        assert states["hello@r1"] == "retained"

        # Roll back, and again, which goes forward to the retained release.

        out = daemon.muructl("rollback", "hello").stdout.strip()
        assert out == "muru: rolled back to hello@r1 (ready)"
        assert greeting(daemon) == "Hello from hello@r1\n"
        assert "app.rolled-back" in daemon.events()
        daemon.muructl("rollback", "hello")
        assert greeting(daemon) == "Hello again from hello@r3\n"
        daemon.muructl("rollback", "hello")
        assert greeting(daemon) == "Hello from hello@r1\n"

        # The configuration as a file.

        exported = daemon.muructl("config", "export").stdout
        assert "[app.hello.release.r1]" in exported
        assert 'entrypoint = "app:application"' in exported

        # Stop and start the daemon: the release serves throughout.

        daemon.stop(shutdown=False)
        assert greeting(daemon) == "Hello from hello@r1\n"
        daemon = start_daemon(tmp_path, run_dir=daemon.run_dir, port=daemon.port)
        assert greeting(daemon) == "Hello from hello@r1\n"
        assert "release.adopted" in daemon.events()

        # Clean up: after a shutdown, the next start serves r1 again.

        daemon.stop()
        daemon = start_daemon(tmp_path, run_dir=daemon.run_dir, port=daemon.port)
        daemon.muructl("release", "wait", "hello@r1")
        assert greeting(daemon) == "Hello from hello@r1\n"
    finally:
        daemon.stop()
