"""The run table and muru.local.toml (design specification section 13.1):
the settings only `muructl run` has live in the project's `[tool.muru.run]`
table, a developer's own `muru.local.toml` lays over it, and a flag wins
over both."""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

import pytest
from support import server as support


def project(directory: Path, run_table: str = "", local: str = "") -> Path:
    """A project serving the plain WSGI fixture, with a run table."""

    directory.mkdir(exist_ok=True)
    shutil.copy(support.FIXTURES / "plain_wsgi.py", directory / "app.py")
    (directory / "pyproject.toml").write_text(
        "[tool.muru]\nentrypoint = 'app:application'\n\n[tool.muru.run]\n" + run_table
    )

    if local:
        (directory / "muru.local.toml").write_text(local)

    return directory


def muructl() -> Path:
    path = support.muructl_path()

    if path is None or not support.extension_available():
        pytest.skip("muructl and the extension must be built")

    return path


def start(directory: Path, *args: str) -> support.Server:
    """Start `muructl run` on the project with no --bind, so the table's
    ports are the ones served, and find them in the serving line."""

    log = Path(tempfile.mkstemp(prefix="muru-run-settings-", suffix=".log")[1])

    with log.open("wb") as handle:
        process = subprocess.Popen(
            [
                str(muructl()),
                "run",
                str(directory),
                "--python",
                sys.executable,
                "--processes",
                "1",
                *args,
            ],
            stdout=handle,
            stderr=subprocess.STDOUT,
        )

    server = support.Server(0, process, log, 1, 1, None)
    deadline = time.monotonic() + 30

    while time.monotonic() < deadline:
        text = server.log()
        serving = re.search(r"muru: serving .* on (.*) with", text)

        if serving:
            http = re.search(r"http://127\.0\.0\.1:(\d+)", serving.group(1))
            https = re.search(r"https://127\.0\.0\.1:(\d+)", serving.group(1))
            assert http is not None, text
            server.port = int(http.group(1))
            server.https_port = int(https.group(1)) if https else None
            return server

        if process.poll() is not None:
            server.stop()
            raise RuntimeError(f"muructl exited:\n{text}")

        time.sleep(0.05)

    server.stop()
    raise RuntimeError(f"muructl did not announce its listeners:\n{server.log()}")


def test_the_run_table_sets_how_the_project_is_served(tmp_path: Path) -> None:
    port = support.free_port()
    https_port = support.free_port()
    directory = project(
        tmp_path / "project",
        f"port = {port}\nhttps = true\nhttps_port = {https_port}\nhosts = ['shop.localhost']\n"
        "reload = false\naccess_log = 'errors'\ndashboard = 'off'\n",
    )
    server = start(directory)

    try:
        assert server.port == port
        assert server.https_port == https_port
        assert server.get("/").status == 200
        assert server.wait_for_log("shop.localhost")
        assert "watching" not in server.log()
        assert "dashboard on" not in server.log()

        # errors: a 200 is not logged, a 404 is.

        assert server.get("/missing").status == 404
        assert server.wait_for_log("404")
        assert " 200" not in server.log()
    finally:
        server.stop()

    # --print-config leaves the table out and writes what it resolved.

    completed = subprocess.run(
        [str(muructl()), "run", str(directory), "--print-config", "--python", sys.executable],
        capture_output=True,
        text=True,
        check=True,
        timeout=30,
    )
    document = tomllib.loads(completed.stdout)
    assert document["listener"] == [
        {"address": f"127.0.0.1:{port}"},
        {"address": f"127.0.0.1:{https_port}", "tls": True},
    ]
    assert "[run]" not in completed.stdout and "[tool.muru.run]" not in completed.stdout


def test_the_local_file_lays_over_the_table_and_a_flag_over_both(tmp_path: Path) -> None:
    table_port = support.free_port()
    local_port = support.free_port()
    flag_port = support.free_port()
    directory = project(
        tmp_path / "project",
        f"port = {table_port}\nhosts = ['table.localhost']\nreload = false\n",
        local=f"[run]\nport = {local_port}\nhosts = ['local.localhost']\nhttps = true\n",
    )
    server = start(directory)

    try:
        assert server.port == local_port
        assert server.https_port is not None
        assert server.wait_for_log("muru: using [run] from")
        assert server.wait_for_log("table.localhost, local.localhost")
        assert "not covered" not in server.log(), "no .gitignore, nothing to say"
    finally:
        server.stop()

    # A .gitignore that does not name the file is said once; one that
    # does is not.

    (directory / ".gitignore").write_text(".venv/\n")
    server = start(directory, "--port", str(flag_port), "--no-https")

    try:
        assert server.port == flag_port
        assert server.https_port is None
        assert server.wait_for_log("muru.local.toml is not covered by the project's .gitignore")
    finally:
        server.stop()

    (directory / ".gitignore").write_text(".venv/\nmuru.local.toml\n")
    server = start(directory)

    try:
        assert server.wait_for_log("muru: using [run] from")
        assert "not covered" not in server.log()
    finally:
        server.stop()


def test_mistakes_name_the_file_they_are_in(tmp_path: Path) -> None:
    directory = project(tmp_path / "project", "port = 'free'\n")
    refused = subprocess.run(
        [str(muructl()), "run", str(directory), "--python", sys.executable],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert refused.returncode != 0
    assert 'pyproject.toml: [run] port = "free" is not a port number or auto' in refused.stderr

    project(directory, "", local="entrypoint = 'other:application'\n")
    refused = subprocess.run(
        [str(muructl()), "run", str(directory), "--python", sys.executable],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert refused.returncode != 0
    assert "entrypoint is not a key of muru.local.toml" in refused.stderr
    assert "belong in" in refused.stderr and "pyproject.toml" in refused.stderr
