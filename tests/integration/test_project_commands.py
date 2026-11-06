"""Commands for the project here (design specification section 9.12).

With no application named, `muructl command run`, `muructl command list`
and `muructl exec` act for the project in the current directory: through
a development run of it when one is up, and by muructl itself otherwise,
as a Kubernetes Job runs a migration. The same through `muructl serve` is
in test_serve.py.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest
from support import server as support

PROJECT = """[tool.muru]
entrypoint = 'app:application'

[tool.muru.env]
GREETING = 'hello'

[tool.muru.commands.greet]
description = 'Say something'
run = ['python', 'tasks.py', 'greet', '{name}', '{status}']
env = { TOKEN = '{token}' }
inputs.name = { required = true }
inputs.status = { default = '0' }
inputs.token = { secret = true }

[tool.muru.commands.report]
call = 'tasks:report'
inputs.target = { default = 'out' }

[tool.muru.commands.hold]
run = ['python', 'tasks.py', 'hold']
exclusive = true

[tool.muru.commands.slow]
run = ['python', 'tasks.py', 'hold']
timeout = '1s'
"""

TASKS = """import os
import signal
import sys
import time
from pathlib import Path


def report(target):
    print(f"report to {target} for {os.environ['MURU_COMMAND']}")
    return 4


if __name__ == "__main__":
    if sys.argv[1] == "greet":
        print(
            os.environ["GREETING"],
            sys.argv[2],
            os.environ["MURU_APP"],
            os.environ.get("MURU_RELEASE", "no-release"),
            os.environ.get("TOKEN") or "no-token",
            Path.cwd().name,
        )
        print("to standard error", file=sys.stderr)
        sys.exit(int(sys.argv[3]))

    # A stop is noted before the signal does what it would have done.

    def stopped(signum, frame):
        Path("hold.stopped").write_text(signal.Signals(signum).name)
        signal.signal(signum, signal.SIG_DFL)
        os.kill(os.getpid(), signum)

    signal.signal(signal.SIGTERM, stopped)
    Path("hold.pid").write_text(str(os.getpid()))
    print("holding", flush=True)
    time.sleep(60)
"""


@pytest.fixture
def project(tmp_path: Path) -> Path:
    directory = tmp_path / "shop"
    directory.mkdir()
    (directory / "pyproject.toml").write_text(PROJECT)
    (directory / "tasks.py").write_text(TASKS)
    (directory / "app.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
    return directory


@pytest.fixture
def runtime() -> Path:
    # A runtime directory of its own, so no run or serve of another test,
    # or of the developer's, is found.

    return support.short_dir("muru-rt-")


def command(runtime: Path, cwd: Path, *arguments: str) -> list[str | Path]:
    path = support.muructl_path()

    if path is None:
        support.unavailable("muructl is not built (cargo build -p muructl)")

    assert path is not None
    return [str(path), *arguments]


def environment(runtime: Path) -> dict[str, str]:
    env = {**os.environ, "MURU_RUN_DIR": str(runtime)}
    env.pop("MURU_SOCKET", None)
    return env


def muructl(
    runtime: Path,
    cwd: Path,
    *arguments: str,
    stdin: str | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a muructl command from `cwd`, with the test's runtime directory."""

    return subprocess.run(
        command(runtime, cwd, *arguments),
        capture_output=True,
        text=True,
        check=False,
        timeout=120,
        cwd=cwd,
        input=stdin,
        stdin=subprocess.DEVNULL if stdin is None else None,
        env=environment(runtime),
    )


def wait_until(condition: object, what: str, timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout

    while not condition():  # type: ignore[operator]
        assert time.monotonic() < deadline, what
        time.sleep(0.05)


def gone(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True

    return False


def test_a_command_runs_on_its_own_when_nothing_serves_the_project(
    project: Path, runtime: Path, tmp_path: Path
) -> None:
    if not support.extension_available():
        support.unavailable("the muru extension is not built into this interpreter (just dev)")

    python = ["--python", sys.executable]

    # The project's commands are listed from its file, with no release
    # and no last run.

    listed = muructl(runtime, project, "command", "list")
    assert listed.returncode == 0, listed.stderr
    assert listed.stdout.startswith(f"app, the project in {project.resolve()}"), listed.stdout
    assert "greet: Say something" in listed.stdout and "last run" not in listed.stdout
    as_json = json.loads(muructl(runtime, project, "command", "list", "--json").stdout)
    assert [entry["name"] for entry in as_json["commands"]] == ["greet", "hold", "report", "slow"]

    # A declared command runs with the project's interpreter, its
    # environment and its directory, and muructl exits with its status.

    ran = muructl(runtime, project, "command", "run", "greet", "--input", "name=world", *python)
    assert ran.returncode == 0, ran.stderr
    assert ran.stdout == "hello world app no-release no-token shop\n"
    assert ran.stderr == "to standard error\n"

    failed = muructl(
        runtime, project, "command", "run", "greet", "--input", "name=x", "--input", "status=3"
    )
    assert failed.returncode == 3
    assert "muru: the command exited with status 3" in failed.stderr

    # From elsewhere the project is named, and an input comes on standard
    # input, a secret one included.

    elsewhere = muructl(
        runtime,
        tmp_path,
        "command",
        "run",
        "greet",
        "--chdir",
        str(project),
        "--input",
        "name=far",
        "--input-stdin",
        "token",
        *python,
        stdin="s3cret\n",
    )
    assert elsewhere.returncode == 0, elsewhere.stderr
    assert elsewhere.stdout == "hello far app no-release s3cret shop\n"

    # A callable is called with its inputs, and its integer is the status.

    called = muructl(runtime, project, "command", "run", "report", "--json", *python)
    assert called.returncode == 4, called.stderr
    lines = [json.loads(line) for line in called.stdout.splitlines()]
    assert lines[0]["line"] == "report to out for report"
    assert lines[-1]["finished"] and lines[-1]["exit_code"] == 4

    # A one-off command gets the same interpreter and environment.

    one_off = muructl(
        runtime,
        project,
        "exec",
        *python,
        "--",
        "python",
        "-c",
        "import os, sys, muru; print(sys.executable, os.environ['GREETING'])",
    )
    assert one_off.returncode == 0, one_off.stderr
    assert one_off.stdout == f"{sys.executable} hello\n"

    # Its timeout stops it, and so does a signal that ends muructl: the
    # command gets SIGTERM, and muructl exits once it has ended.

    slow = muructl(runtime, project, "command", "run", "slow", *python)
    assert slow.returncode == 128 + signal.SIGTERM
    assert "ran past its timeout of 1s" in slow.stderr
    assert (project / "hold.stopped").read_text() == "SIGTERM"
    (project / "hold.pid").unlink()
    (project / "hold.stopped").unlink()

    held = subprocess.Popen(
        command(runtime, project, "command", "run", "hold", *python),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        stdin=subprocess.DEVNULL,
        text=True,
        cwd=project,
        env=environment(runtime),
    )

    try:
        wait_until((project / "hold.pid").exists, "the command never started")
        pid = int((project / "hold.pid").read_text())
        held.send_signal(signal.SIGTERM)
        _, stderr = held.communicate(timeout=30)
        assert held.returncode == 128 + signal.SIGTERM
        assert gone(pid)
        assert (project / "hold.stopped").read_text() == "SIGTERM"
        assert "muru: the command stopped because muructl got SIGTERM" in stderr, stderr
    finally:
        held.kill()

    # A muructl serve that left its files behind is passed over.

    stale = support.short_dir("muru-stale-")
    (stale / "admin.sock").write_text("")
    (stale / "serve.json").write_text(
        json.dumps(
            {
                "pid": 1,
                "app": "app",
                "project": str(project.resolve()),
                "listen": "127.0.0.1:8080",
                "socket": str(stale / "admin.sock"),
            }
        )
    )
    passed = muructl(stale, project, "command", "run", "greet", "--input", "name=again", *python)
    assert passed.returncode == 0, passed.stderr
    assert passed.stdout.startswith("hello again app no-release")

    # What cannot be run says why.

    for arguments, message in [
        (["command", "run", "nope"], "has no command named nope; it declares greet, hold"),
        (["command", "run", "greet"], "greet needs the input name"),
        (
            ["command", "run", "greet", "--input", "name=x", "--secret-input", "token=vault/key"],
            "--input-stdin token",
        ),
        (
            ["command", "run", "shop", "greet", "--chdir", str(project)],
            "--chdir finds the project a command runs for when no application is named",
        ),
    ]:
        refused = muructl(runtime, project, *arguments)
        assert refused.returncode != 0
        assert message in refused.stderr, refused.stderr

    empty = muructl(runtime, tmp_path, "command", "run", "greet")
    assert empty.returncode != 0
    assert "has no pyproject.toml or muru.toml to declare greet in" in empty.stderr


def test_a_command_runs_through_a_development_run_of_the_project(
    project: Path, runtime: Path, tmp_path: Path
) -> None:
    server = support.start(
        str(project),
        workers=1,
        threads=2,
        chdir=None,
        env={"MURU_RUN_DIR": str(runtime)},
    )

    try:
        assert server.wait_for_log("admin socket"), server.log()

        # The run is found from the project, and the command's output and
        # events join the run's own.

        ran = muructl(runtime, project, "command", "run", "greet", "--input", "name=dev")
        assert ran.returncode == 0, ran.stderr
        assert ran.stdout == "hello dev app r1 no-token shop\n"
        assert ran.stderr == "to standard error\n"
        assert server.wait_for_log("command.finished: greet: exited with status 0"), server.log()
        log = server.log()
        assert "[command greet] hello dev app r1" in log
        assert "command.started: greet: started (pid " in log and "with name=dev" in log
        shown = muructl(runtime, project, "logs").stdout
        assert "[command greet] hello dev app r1" in shown

        # Named from elsewhere, or by its application, it is the same run.

        elsewhere = muructl(
            runtime, tmp_path, "exec", "--chdir", str(project), "--", "python", "tasks.py",
            "greet", "far", "5",
        )  # fmt: skip
        assert elsewhere.returncode == 5, elsewhere.stderr
        assert elsewhere.stdout == "hello far app r1 no-token shop\n"
        assert server.wait_for_log("command.failed: exec: exited with status 5"), server.log()
        named = muructl(runtime, project, "command", "list", "app")
        assert named.stdout.startswith("app@r1\n"), named.stdout + named.stderr

        # An exclusive command runs one at a time for the run, and ends
        # with its caller.

        held = subprocess.Popen(
            command(runtime, project, "command", "run", "hold"),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            stdin=subprocess.DEVNULL,
            text=True,
            cwd=project,
            env=environment(runtime),
        )

        try:
            wait_until((project / "hold.pid").exists, "the command never started")
            pid = int((project / "hold.pid").read_text())
            second = muructl(runtime, project, "command", "run", "hold")
            assert second.returncode != 0
            assert "hold runs one at a time for app, and a run is under way" in second.stderr
        finally:
            held.kill()
            held.wait()

        wait_until(lambda: gone(pid), "the command outlived its caller")
        assert server.wait_for_log("command.failed: hold: stopped because its caller went away"), (
            server.log()
        )
    finally:
        server.stop()
