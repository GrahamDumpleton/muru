"""Commands an application declares (design specification section 9.12):
listed with their inputs and last runs, run by name with inputs checked,
secret inputs kept out of events, callables called, and exclusive
commands and timeouts enforced."""

from __future__ import annotations

import json
import subprocess
import textwrap
import time
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon

PYPROJECT = """
[project]
name = "ops"
version = "0.1.0"

[tool.muru]
entrypoint = "app:application"
processes = 1

[tool.muru.commands.greet]
description = "Say hello"
run = [
    "python", "-c",
    "import os, sys; print('hello', sys.argv[1], os.environ['SECRET_WORD'])",
    "{who}",
]
env = { SECRET_WORD = "{word}" }
inputs.who = { required = true, pattern = "[a-z]+", description = "Whom to greet" }
inputs.word = { secret = true }

[tool.muru.commands.backup]
call = "ops:backup"
inputs.target = { default = "s3://backups" }
inputs.label = {}

[tool.muru.commands.slow]
run = ["python", "-c", "import time; print('up', flush=True); time.sleep(60)"]
exclusive = true
timeout = "2s"

[tool.muru.commands.hold]
run = ["python", "-c", "import time; print('up', flush=True); time.sleep(60)"]
"""

APP = """
def application(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"ok"]
"""

OPS = """
import os


def backup(target, label="none"):
    print("backing up to", target, "as", label, "for", os.environ["MURU_COMMAND"])
    return 0 if target.startswith("s3://") else 4
"""


def deployed(tmp_path: Path) -> Daemon:
    project = tmp_path / "ops"
    project.mkdir()
    (project / "pyproject.toml").write_text(textwrap.dedent(PYPROJECT))
    (project / "app.py").write_text(APP)
    (project / "ops.py").write_text(OPS)
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(
        tmp_path / "daemon",
        config_extra="[secrets.store.local]\nkind = 'file'\n",
    )
    assert daemon.deploy("ops", project) == "muru: release ops@r1 is ready and serving"
    return daemon


def test_declared_commands_run_with_their_inputs(tmp_path: Path) -> None:
    daemon = deployed(tmp_path)

    try:
        listed = daemon.muructl("command", "list", "ops").stdout
        assert "greet: Say hello" in listed, listed
        assert "input who (required, matching [a-z]+): Whom to greet" in listed, listed
        assert "input word (secret)" in listed, listed
        assert "call: ops:backup" in listed, listed
        assert "timeout 2s, one run at a time" in listed, listed

        ran = daemon.muructl(
            "command", "run", "ops", "greet", "--input", "who=bob", "--input", "word=swordfish"
        )
        assert ran.stdout.strip() == "hello bob swordfish", ran

        # The secret's value is in no event.

        events = daemon.muructl("events").stdout
        assert "greet: started" in events and "who=bob, word=***" in events, events
        assert "swordfish" not in events

        # Inputs are checked before anything runs.

        missing = daemon.muructl("command", "run", "ops", "greet", check=False)
        assert missing.returncode != 0
        assert "greet needs the input who" in missing.stderr, missing.stderr

        bad = daemon.muructl("command", "run", "ops", "greet", "--input", "who=Bob", check=False)
        assert '"Bob" does not match [a-z]+' in bad.stderr, bad.stderr

        unknown = daemon.muructl("command", "run", "ops", "nothing", check=False)
        assert "ops@r1 has no command named nothing; it declares backup, greet, hold, slow" in (
            unknown.stderr
        )

        # A value can come from standard input, or from a secret store
        # without passing through the caller at all.

        piped = daemon.muructl(
            "command",
            "run",
            "ops",
            "greet",
            "--input",
            "who=amy",
            "--input-stdin",
            "word",
            stdin="from-stdin\n",
        )
        assert piped.stdout.strip() == "hello amy from-stdin", piped

        daemon.muructl("secret", "set", "local", "word", stdin="from-the-store")
        stored = daemon.muructl(
            "command",
            "run",
            "ops",
            "greet",
            "--input",
            "who=sam",
            "--secret-input",
            "word=local/word",
        )
        assert stored.stdout.strip() == "hello sam from-the-store", stored

        # The list says how each command last went.

        listed = daemon.muructl("command", "list", "ops", "--json").stdout
        commands = {command["name"]: command for command in json.loads(listed)["commands"]}
        assert commands["greet"]["last_run"]["ok"] is True, commands
        assert commands["greet"]["last_run"]["release"] == "r1"
        assert commands["backup"].get("last_run") is None
    finally:
        daemon.stop()


def test_a_callable_is_called_with_the_inputs(tmp_path: Path) -> None:
    daemon = deployed(tmp_path)

    try:
        ran = daemon.muructl("command", "run", "ops", "backup")
        assert ran.stdout.strip() == "backing up to s3://backups as none for backup", ran

        ran = daemon.muructl(
            "command",
            "run",
            "ops",
            "backup",
            "--input",
            "target=/tmp/x",
            "--input",
            "label=b1",
            check=False,
        )
        assert ran.returncode == 4, ran
        assert "backing up to /tmp/x as b1" in ran.stdout, ran
        assert "muru: the command exited with status 4" in ran.stderr, ran
    finally:
        daemon.stop()


def test_exclusive_commands_run_one_at_a_time_and_time_out(tmp_path: Path) -> None:
    daemon = deployed(tmp_path)
    muructl = support.muructl_path()
    assert muructl is not None
    socket = str(daemon.run_dir / "admin.sock")

    try:
        first = subprocess.Popen(
            [str(muructl), "command", "run", "ops", "slow", "--socket", socket],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        assert first.stdout is not None
        assert first.stdout.readline().strip() == "up"

        second = daemon.muructl("command", "run", "ops", "slow", check=False)
        assert "slow runs one at a time for ops, and a run is under way" in second.stderr, second

        # The first runs past its timeout and is stopped; then the
        # command may run again.

        _, stderr = first.communicate(timeout=30)
        assert first.returncode == 143, (first.returncode, stderr)
        assert "stopped because it ran past its timeout of 2s" in stderr, stderr

        deadline = time.monotonic() + 10

        while True:
            again = daemon.muructl("command", "run", "ops", "slow", "--json", check=False)

            if "one at a time" not in again.stdout:
                break

            assert time.monotonic() < deadline, again
            time.sleep(0.2)

        assert json.loads(again.stdout.splitlines()[0])["line"] == "up", again.stdout

        # The daemon shutting down stops a command running under it, and
        # its caller hears how it ended.

        held = subprocess.Popen(
            [str(muructl), "command", "run", "ops", "hold", "--socket", socket],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        assert held.stdout is not None
        assert held.stdout.readline().strip() == "up"
        daemon.stop()
        _, stderr = held.communicate(timeout=30)
        assert held.returncode == 143, (held.returncode, stderr)
        assert "stopped because the daemon shut down" in stderr, stderr
    finally:
        daemon.stop()
