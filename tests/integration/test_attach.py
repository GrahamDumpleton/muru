"""Debugger attach (design specification section 11.7): `breakpoint()` in a
worker waits for `muructl debug attach`, which carries a pdb session line
by line through the daemon and the host; with no breakpoint waiting the
session gets a thread of its own. A debugger runs code in the worker, so
it is off unless the daemon's file allows exec.
"""

from __future__ import annotations

import threading
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture

APP = """
def application(environ, start_response):
    value = 20

    if environ["PATH_INFO"] == "/pause":
        breakpoint(timeout=float(environ.get("QUERY_STRING") or 30))

    start_response("200 OK", [("Content-Type", "text/plain")])
    return [f"value={value}".encode()]
"""


def wait_for(predicate: Callable[[], bool], timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def deploy_paused(daemon: Daemon, source: Path) -> None:
    source.mkdir(exist_ok=True)
    (source / "paused.py").write_text(APP)
    assert daemon.deploy(
        "paused",
        source,
        "--entrypoint",
        "paused:application",
        "--processes",
        "2",
        "--threads",
        "2",
    ) == ("muru: release paused@r1 is ready and serving")


def in_background(
    call: Callable[[], support.Response],
) -> tuple[threading.Thread, list[support.Response]]:
    answers: list[support.Response] = []
    thread = threading.Thread(target=lambda: answers.append(call()))
    thread.start()
    return thread, answers


def test_attach_is_refused_unless_exec_is_allowed(daemon: Daemon, tmp_path: Path) -> None:
    deploy_paused(daemon, tmp_path / "source")
    refused = daemon.muructl("debug", "attach", "paused", "--worker", "0", check=False, stdin="")
    assert refused.returncode != 0
    assert "debugger attach is off" in refused.stderr


def test_a_session_debugs_the_request_at_its_breakpoint(tmp_path: Path) -> None:
    allowed = start_daemon(tmp_path, config_extra="[daemon]\ndebug_exec = true\n")

    try:
        deploy_paused(allowed, tmp_path / "source")
        thread, answers = in_background(lambda: allowed.get("/pause"))
        wait_for(lambda: "debug.breakpoint" in allowed.events())

        # Without --worker the session goes to the worker that waits.

        session = allowed.muructl(
            "debug", "attach", "paused", stdin="p value\nvalue = 99\ncontinue\n"
        )
        assert "muru: attached to worker" in session.stderr
        assert "paused.py" in session.stderr
        assert "(Pdb) 20" in session.stdout.replace("\n", " ")
        thread.join(30)
        assert answers[0].body == b"value=99"

        kinds = allowed.events()
        assert "debug.attached" in kinds and "debug.detached" in kinds

        # With none waiting, the session has a thread of its own, and the
        # end of input ends it.

        shell = allowed.muructl("debug", "attach", "paused", "--worker", "1", stdin="p 6 * 7\n")
        assert "a thread of its own" in shell.stderr
        assert "42" in shell.stdout

        # Nobody attaching, the request carries on after the timeout.

        started = time.monotonic()
        assert allowed.get("/pause?1").body == b"value=20"
        assert time.monotonic() - started >= 1
        wait_for(lambda: "debug.breakpoint_expired" in allowed.events())
    finally:
        allowed.stop()
