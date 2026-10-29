"""The admin socket of a development run (design specification sections
11.7, 13.1 and 13.8): `muructl run` serves the admin protocol, so the
commands that look into the workers or read the edge's figures work
against it unchanged, the socket is found from the project directory
without a flag, and the commands that manage the daemon's state say so."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import urllib.request
from pathlib import Path

import pytest
from support import server as support


@pytest.fixture
def runtime() -> Path:
    # A runtime directory of its own, short enough for the socket path
    # limit, as the daemon tests make theirs.

    return Path(tempfile.mkdtemp(prefix="muru-rt-", dir="/tmp"))


def muructl(
    runtime: Path,
    *arguments: str,
    cwd: Path | None = None,
    check: bool = True,
    stdin: str | None = None,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a muructl command with the test's runtime directory."""

    path = support.muructl_path()
    assert path is not None
    completed = subprocess.run(
        [str(path), *arguments],
        capture_output=True,
        text=True,
        check=False,
        timeout=120,
        cwd=cwd,
        input=stdin,
        env={**os.environ, "MURU_RUN_DIR": str(runtime), **(env or {})},
    )

    if check and completed.returncode != 0:
        raise RuntimeError(
            f"muructl {' '.join(arguments)} failed:\n{completed.stdout}{completed.stderr}"
        )

    return completed


def start(runtime: Path, port: int | None = None, **kwargs: object) -> support.Server:
    args = list(kwargs.pop("args", []))

    if port is not None:
        args += ["--dashboard", f"127.0.0.1:{port}"]

    server = support.start(
        "plain_wsgi:application",
        workers=2,
        threads=2,
        args=args,
        env={"MURU_RUN_DIR": str(runtime)},
        **kwargs,  # type: ignore[arg-type]
    )
    assert server.wait_for_log("admin socket"), server.log()
    return server


def test_the_commands_reach_a_development_run(runtime: Path) -> None:
    server = start(runtime)

    try:
        assert server.get("/").status == 200

        # From the project directory the socket needs no flag, and status
        # says what was found.

        status = muructl(runtime, "status", cwd=support.FIXTURES).stdout
        assert status.startswith("run      pid ")
        assert "app      plain-wsgi@r1" in status
        assert f"project  {support.FIXTURES.resolve()}" in status
        assert f"socket   {runtime}/runs/{server.port}/admin.sock" in status
        assert "serving  http://127.0.0.1:" in status

        # The debug commands ask the run's workers, by the application
        # the terminal names, with no debug_exec needed for exec.

        rows = muructl(runtime, "debug", "status", "plain-wsgi", cwd=support.FIXTURES).stdout
        assert rows.splitlines()[0].split()[:3] == ["WORKER", "PID", "GROUP"]
        assert len(rows.splitlines()) == 3
        stacks = muructl(runtime, "debug", "stacks", "plain-wsgi@r1", cwd=support.FIXTURES).stdout
        assert "MainThread in 2 workers" in stacks
        ran = muructl(
            runtime,
            "debug",
            "exec",
            "plain-wsgi",
            "--worker",
            "0",
            "--",
            "print(6 * 7)",
            cwd=support.FIXTURES,
        ).stdout
        assert "42" in ran
        assert server.wait_for_log("muru: debug.exec: ran code in worker 0")

        # The edge's figures: metrics, the access log and the WebSockets.

        metrics = muructl(runtime, "metrics", cwd=support.FIXTURES).stdout
        assert "muru_requests_total{" in metrics
        access = muructl(runtime, "logs", "--access", "-n", "5", cwd=support.FIXTURES).stdout
        assert "200 GET / " in access
        raw = muructl(runtime, "logs", "--access", "--json", "-n", "1", cwd=support.FIXTURES).stdout
        assert json.loads(raw)["status"] == 200
        output = muructl(runtime, "logs", "plain-wsgi", cwd=support.FIXTURES).stdout
        assert "muru.host[" in output and "is ready" in output
        assert "no WebSockets open" in muructl(runtime, "websockets", cwd=support.FIXTURES).stdout

        # Logging levels and a restart go through the host as they do
        # under the daemon; the restart's event reaches the terminal.

        changed = muructl(
            runtime, "logging", "level", "plain-wsgi", "=DEBUG", "--for", "1m", cwd=support.FIXTURES
        ).stdout
        assert "DEBUG" in changed
        restarted = muructl(
            runtime, "restart", "plain-wsgi", "--worker", "1", cwd=support.FIXTURES
        ).stdout
        assert "worker 1" in restarted and "is being replaced by pid" in restarted
        assert server.wait_for_log("muru: worker.restarted:")

        # What manages the daemon's state says it needs the daemon.

        refused = muructl(runtime, "deploy", "shop", ".", cwd=support.FIXTURES, check=False)
        assert refused.returncode != 0
        assert "muructl deploy needs the daemon: this is the development run of plain-wsgi" in (
            refused.stderr
        )
        refused = muructl(runtime, "events", cwd=support.FIXTURES, check=False)
        assert "a development run keeps no event log" in refused.stderr
        refused = muructl(runtime, "logs", "reopen", cwd=support.FIXTURES, check=False)
        assert "writes no log files" in refused.stderr

        # Another application, or a release that is not serving, is not
        # found.

        missing = muructl(runtime, "debug", "status", "shop", cwd=support.FIXTURES, check=False)
        assert "no application shop here" in missing.stderr
        missing = muructl(
            runtime, "debug", "status", "plain-wsgi@r7", cwd=support.FIXTURES, check=False
        )
        assert "plain-wsgi@r7 is not serving" in missing.stderr
    finally:
        server.stop()

    # The run's place in the runtime directory goes with it.

    assert not (runtime / "runs" / str(server.port)).exists()


def test_the_socket_is_found_by_project_or_named(runtime: Path, tmp_path: Path) -> None:
    other = tmp_path / "other"
    other.mkdir()
    (other / "plain_wsgi.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
    first = start(runtime)
    second = start(runtime, chdir=other)

    try:
        # Each project's directory finds its own run.

        assert (
            f"runs/{first.port}/admin.sock"
            in muructl(runtime, "status", cwd=support.FIXTURES).stdout
        )
        assert f"runs/{second.port}/admin.sock" in muructl(runtime, "status", cwd=other).stdout

        # Elsewhere, with no daemon, the runs are named so one can be chosen.

        ambiguous = muructl(runtime, "status", cwd=tmp_path, check=False)
        assert ambiguous.returncode != 0
        assert "several development runs are" in ambiguous.stderr
        assert f"--socket {runtime}/runs/{first.port}/admin.sock" in ambiguous.stderr
        assert f"--socket {runtime}/runs/{second.port}/admin.sock" in ambiguous.stderr
        chosen = muructl(
            runtime,
            "status",
            "--socket",
            f"{runtime}/runs/{second.port}/admin.sock",
            cwd=tmp_path,
        ).stdout
        assert f"project  {other.resolve()}" in chosen
        chosen = muructl(
            runtime,
            "status",
            cwd=tmp_path,
            env={"MURU_SOCKET": f"{runtime}/runs/{first.port}/admin.sock"},
        ).stdout
        assert f"project  {support.FIXTURES.resolve()}" in chosen
    finally:
        first.stop()
        second.stop()

    # With none running, the command says so, naming both ways to start one.

    nothing = muructl(runtime, "status", cwd=tmp_path, check=False)
    assert "nothing is listening at" in nothing.stderr
    assert "no development run (muructl run)" in nothing.stderr


def test_the_dashboard_has_the_worker_pages(runtime: Path) -> None:
    port = support.free_port()
    server = start(runtime, port=port)

    def read(path: str) -> str:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=10) as response:
            body: bytes = response.read()
            return body.decode()

    try:
        assert server.wait_for_log(f"dashboard on http://127.0.0.1:{port}/")
        assert 'href="workers?release=plain-wsgi@r1"' in read("/fragment/overview")
        page = read("/workers?release=plain-wsgi@r1")
        assert "<title>Muru: plain-wsgi@r1 workers</title>" in page
        assert '<a href="./">run</a>' in page
        assert "Restart" not in page
        status = json.loads(read("/v1/debug/status?release=plain-wsgi@r1"))
        assert [worker["index"] for worker in status["workers"]] == [0, 1]
        assert all(worker["result"]["ready"] for worker in status["workers"])
        stacks = read("/fragment/workers/stacks?release=plain-wsgi@r1")
        assert "MainThread" in stacks

        with pytest.raises(urllib.error.HTTPError) as refused:
            read("/v1/debug/status?release=plain-wsgi@r9")

        assert refused.value.code == 409
        assert "is not serving" in refused.value.read().decode()
    finally:
        server.stop()
