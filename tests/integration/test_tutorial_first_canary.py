"""The steps of the first canary tutorial, with the tutorial's own releases.

docs/tutorials/first-canary.md includes the releases under
docs/tutorials/code/releases, and this test runs their canaries through
the page's steps, so a change that breaks one fails here rather than for
a reader. The good release's steps hold for 2 s rather than the page's
30 s, to keep the test short.
"""

from __future__ import annotations

import threading
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon

RELEASES = support.ROOT / "docs" / "tutorials" / "code" / "releases"


@contextmanager
def loop(daemon: Daemon) -> Iterator[list[bytes]]:
    """Ask for the page over and over, as the page's curl loop does."""

    bodies: list[bytes] = []
    stop = threading.Event()

    # Each request is a new connection, so the loop is paced to stay
    # well inside the local ports a busy machine has free.

    def run() -> None:
        while not stop.wait(0.01):
            try:
                bodies.append(daemon.get().body)
            except OSError:
                continue

    runners = [threading.Thread(target=run) for _ in range(4)]

    for runner in runners:
        runner.start()

    try:
        yield bodies
    finally:
        stop.set()

        for runner in runners:
            runner.join()


def greetings(daemon: Daemon, count: int = 20) -> set[str]:
    return {daemon.get().body.decode() for _ in range(count)}


def test_the_first_canary_tutorial(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)

    try:
        # Where the deploy tutorial left off: r1 serving after r2 failed
        # and r3 was rolled back.

        daemon.deploy("hello", RELEASES / "v1", "--entrypoint", "app:application")
        daemon.muructl("deploy", "hello", str(RELEASES / "v2"), check=False)
        daemon.muructl("deploy", "hello", str(RELEASES / "v3"))
        daemon.muructl("rollback", "hello")
        assert greetings(daemon) == {"Hello from hello@r1\n"}

        # A release with a bug, held out of traffic.

        held = daemon.muructl("deploy", "hello", str(RELEASES / "v4"), "--hold").stdout
        assert held.strip() == "muru: release hello@r4 is ready and held: not serving"
        assert greetings(daemon) == {"Hello from hello@r1\n"}

        # Its canary is rolled back by the error rate gate, long before
        # the step's hold is over.

        with loop(daemon) as bodies:
            started = daemon.muructl(
                "rollout", "canary", "hello@r4", "--steps", "10,50", "--hold", "30s"
            ).stdout
            assert "rollout 1 of hello@r4 started against r1: step 1 of 2: 10% for 30s" in started
            followed = daemon.muructl("rollout", "status", "hello", "--wait").stdout

        assert "rollout 1 rolled_back: gate failed at 10%: 5xx rate" in followed
        assert b"Howdy from hello@r4\n" in bodies
        assert any(body.startswith(b"500 Internal Server Error") for body in bodies)
        assert "rollout.gate_failed" in daemon.events()
        assert greetings(daemon) == {"Hello from hello@r1\n"}

        # A good release: previewed during its first step, its second
        # step approved, then promoted.

        daemon.muructl("deploy", "hello", str(RELEASES / "v5"), "--hold")

        with loop(daemon):
            daemon.muructl(
                "rollout",
                "canary",
                "hello@r5",
                "--steps",
                "10,50",
                "--hold",
                "2s",
                "--min-requests",
                "20",
                "--approve-each-step",
            )
            preview = daemon.request("/", headers={"Muru-Preview": "r5"})
            assert preview.body == b"Howdy from hello@r5\n"

            deadline = time.monotonic() + 30
            status = ""

            while "awaiting_approval" not in status and time.monotonic() < deadline:
                status = daemon.muructl("rollout", "status", "hello").stdout
                time.sleep(0.2)

            assert "awaits approval: muructl rollout resume hello" in status
            daemon.muructl("rollout", "resume", "hello")
            followed = daemon.muructl("rollout", "status", "hello", "--wait").stdout

        assert "rollout 2 promoted: every step passed its gates" in followed
        assert greetings(daemon) == {"Howdy from hello@r5\n"}
    finally:
        daemon.stop()
