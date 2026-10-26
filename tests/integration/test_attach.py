"""Debugger attach (design specification section 11.7): `breakpoint()` in a
worker waits for `muructl debug attach`, which carries a pdb session line
by line through the daemon and the host; with no breakpoint waiting the
session gets a thread of its own. While a session is open its worker is
out of routing and pinned. With --dap the session carries the Debug
Adapter Protocol between an IDE and debugpy in the worker. A debugger
runs code in the worker, so it is off unless the daemon's file allows
exec.
"""

from __future__ import annotations

import json
import socket
import subprocess
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any, BinaryIO

from support import server as support
from support.daemon import Daemon, start_daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture

APP = """
import os


def application(environ, start_response):
    value = 20

    if environ["PATH_INFO"] == "/pid":
        start_response("200 OK", [("Content-Type", "text/plain")])
        return [str(os.getpid()).encode()]

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


def worker_states(daemon: Daemon) -> dict[int, tuple[str, str]]:
    """Each worker's pid and state, from muructl debug status, by index."""

    states = {}

    for line in daemon.muructl("debug", "status", "paused").stdout.splitlines()[1:]:
        fields = line.split()
        states[int(fields[0])] = (fields[1], fields[-1])

    return states


def test_a_debugged_worker_leaves_routing_until_the_session_ends(tmp_path: Path) -> None:
    allowed = start_daemon(tmp_path, config_extra="[daemon]\ndebug_exec = true\n")
    muructl = support.muructl_path()
    assert muructl is not None

    def served_by() -> set[str]:
        return {allowed.get("/pid").body.decode() for _ in range(8)}

    try:
        deploy_paused(allowed, tmp_path / "source")
        debugged = worker_states(allowed)[1][0]
        session = subprocess.Popen(
            [
                str(muructl),
                "debug",
                "attach",
                "paused",
                "--worker",
                "1",
                "--socket",
                str(allowed.run_dir / "admin.sock"),
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            text=True,
        )

        try:
            wait_for(lambda: worker_states(allowed)[1][1] == "debugging")
            wait_for(lambda: debugged not in served_by())
            assert debugged not in served_by()
        finally:
            assert session.stdin is not None
            session.stdin.close()
            session.wait(30)

        # The session over, the same worker serves again.

        wait_for(lambda: worker_states(allowed)[1] == (debugged, "ready"))
        wait_for(lambda: debugged in served_by())
    finally:
        allowed.stop()


class Ide:
    """Just enough of an IDE to speak the Debug Adapter Protocol."""

    def __init__(self, port: int) -> None:
        deadline = time.monotonic() + 20

        while True:
            try:
                self.connection = socket.create_connection(("127.0.0.1", port), timeout=30)
                break
            except OSError:
                assert time.monotonic() < deadline, "muructl did not listen"
                time.sleep(0.1)

        self.stream: BinaryIO = self.connection.makefile("rb")
        self.seq = 0

    def send(self, command: str, arguments: dict[str, Any] | None = None) -> None:
        self.seq += 1
        body = json.dumps(
            {"seq": self.seq, "type": "request", "command": command, "arguments": arguments or {}}
        ).encode()
        self.connection.sendall(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)

    def receive(self) -> dict[str, Any]:
        length = 0

        while (line := self.stream.readline().strip()) != b"":
            name, _, value = line.partition(b":")

            if name.lower() == b"content-length":
                length = int(value)

        message: dict[str, Any] = json.loads(self.stream.read(length))
        return message

    def until(self, kind: str, name: str) -> dict[str, Any]:
        """Read messages until a response to `name` or an event called `name`."""

        key = "command" if kind == "response" else "event"

        while True:
            message = self.receive()

            if message.get("type") == kind and message.get(key) == name:
                return message

    def close(self) -> None:
        self.stream.close()
        self.connection.close()


def test_an_ide_debugs_a_breakpoint_through_dap(tmp_path: Path) -> None:
    allowed = start_daemon(tmp_path, config_extra="[daemon]\ndebug_exec = true\n")
    muructl = support.muructl_path()
    assert muructl is not None
    port = support.free_port()

    try:
        deploy_paused(allowed, tmp_path / "source")
        thread, answers = in_background(lambda: allowed.get("/pause"))
        wait_for(lambda: "debug.breakpoint" in allowed.events())
        session = subprocess.Popen(
            [
                str(muructl),
                "debug",
                "attach",
                "paused",
                "--dap",
                "--listen",
                f"127.0.0.1:{port}",
                "--socket",
                str(allowed.run_dir / "admin.sock"),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        ide = Ide(port)

        try:
            ide.send("initialize", {"clientID": "muru-test", "adapterID": "debugpy"})
            assert ide.until("response", "initialize")["success"]
            ide.send("attach", {"justMyCode": True})
            ide.until("event", "initialized")
            ide.send("configurationDone")

            # The request stops at the line after its breakpoint().

            stopped = ide.until("event", "stopped")
            ide.send("stackTrace", {"threadId": stopped["body"]["threadId"]})
            frame = ide.until("response", "stackTrace")["body"]["stackFrames"][0]
            assert frame["source"]["path"].endswith("paused.py"), frame
            assert frame["name"] == "application"

            ide.send("continue", {"threadId": stopped["body"]["threadId"]})
            ide.until("response", "continue")
            thread.join(30)
            assert answers[0].body == b"value=20"
        finally:
            ide.close()
            _, errors = session.communicate(timeout=30)

        assert "attached to worker" in errors and "has ended" in errors, errors
        wait_for(lambda: all(state == "ready" for _, state in worker_states(allowed).values()))
    finally:
        allowed.stop()
