"""One-off commands with a release (design specification section 9.12):
muructl exec runs a command with the release's interpreter, environment
and source directory, streams its output and exits with its status."""

from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

from support import server as support
from support.daemon import SHOP, Daemon, start_daemon

PRINT = (
    "import os, sys; "
    "print(os.environ['GREETING'], os.environ['MURU_APP'], os.environ['MURU_RELEASE']); "
    "print(os.getcwd()); "
    "print(sys.executable); "
    "print('to stderr', file=sys.stderr)"
)


def started(tmp_path: Path) -> Daemon:
    daemon = start_daemon(tmp_path)
    deployed = daemon.deploy("shop", SHOP, "--processes", "1", "--env", "GREETING=hello")
    assert deployed == "muru: release shop@r1 is ready and serving"
    return daemon


def test_exec_runs_with_the_release_and_streams_its_output(tmp_path: Path) -> None:
    daemon = started(tmp_path)

    try:
        ran = daemon.muructl("exec", "shop", "--", "python", "-c", PRINT)
        lines = ran.stdout.splitlines()
        assert lines[0] == "hello shop r1", ran.stdout
        assert Path(lines[1]).resolve() == SHOP.resolve(), ran.stdout
        assert Path(lines[2]).resolve() == Path(sys.executable).resolve(), ran.stdout
        assert ran.stderr.strip() == "to stderr", ran.stderr

        # The output went into the release's log, and the run is in the
        # events.

        logs = daemon.muructl("logs", "shop", "--lines", "50").stdout
        assert "[command exec] hello shop r1" in logs, logs
        events = daemon.muructl("events").stdout
        assert "command.started" in events and "command.finished" in events, events
        assert "exec: exited with status 0 after" in events, events
    finally:
        daemon.stop()


def test_exec_exits_with_the_command_status(tmp_path: Path) -> None:
    daemon = started(tmp_path)

    try:
        failed = daemon.muructl(
            "exec", "shop@r1", "--", "python", "-c", "raise SystemExit(3)", check=False
        )
        assert failed.returncode == 3, failed
        assert "muru: the command exited with status 3 after" in failed.stderr, failed.stderr
        assert "command.failed" in daemon.muructl("events").stdout

        missing = daemon.muructl("exec", "shop", "--", "no-such-program", check=False)
        assert missing.returncode == 127, missing
        assert "no-such-program could not be started" in missing.stderr, missing.stderr

        # --json prints each line and the ending as the admin API's
        # command output.

        printed = daemon.muructl("exec", "shop", "--json", "--", "python", "-c", "print('hi')")
        messages = [json.loads(line) for line in printed.stdout.splitlines()]
        assert messages[0]["line"] == "hi" and messages[0]["release"] == "r1", messages
        assert messages[-1]["finished"] is True, messages
        assert messages[-1]["exit_code"] == 0, messages

        refused = daemon.muructl("exec", "nothing", "--", "true", check=False)
        assert refused.returncode != 0 and "nothing" in refused.stderr, refused.stderr
    finally:
        daemon.stop()


def test_a_command_stops_when_its_caller_goes_away(tmp_path: Path) -> None:
    daemon = started(tmp_path)
    muructl = support.muructl_path()
    assert muructl is not None
    sleeper = "import time; print('up', flush=True); time.sleep(120)"
    socket = str(daemon.run_dir / "admin.sock")

    try:
        caller = subprocess.Popen(
            [str(muructl), "exec", "shop", "--socket", socket, "--", "python", "-c", sleeper],
            stdout=subprocess.PIPE,
            text=True,
        )
        assert caller.stdout is not None
        assert caller.stdout.readline().strip() == "up"
        caller.kill()
        caller.wait()

        # The daemon notices the stream closing and stops the command.

        deadline = time.monotonic() + 30
        stopped = "exec: stopped because its caller went away"

        while stopped not in (events := daemon.muructl("events").stdout):
            assert time.monotonic() < deadline, events
            time.sleep(0.2)

        assert "command.failed" in events
    finally:
        daemon.stop()
