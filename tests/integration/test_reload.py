"""`muructl run --reload`: a changed source file starts a new release (section 9.8)."""

from __future__ import annotations

import shutil
import textwrap
import time
from pathlib import Path

import pytest
from support import server as support

APP = """
import os

VERSION = {version!r}


def application(environ, start_response):
    body = f"{{VERSION}} from {{os.getpid()}}".encode()
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", str(len(body)))])
    return [body]
"""


def write_app(directory: Path, version: str) -> None:
    (directory / "reload_app.py").write_text(textwrap.dedent(APP.format(version=version)))


def version_seen(server: support.Server) -> str:
    return server.get("/").body.decode().split(" from ")[0]


def wait_for_version(server: support.Server, version: str, timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if version_seen(server) == version:
            return
        time.sleep(0.1)

    raise AssertionError(f"version {version!r} did not appear; log:\n{server.log()}")


@pytest.fixture
def project(tmp_path: Path) -> Path:
    directory = tmp_path / "project"
    directory.mkdir()
    write_app(directory, "one")
    shutil.copy(support.FIXTURES / "plain_wsgi.py", directory / "unrelated.py")
    return directory


def test_source_changes_start_a_new_release(project: Path) -> None:
    server = support.start("reload_app:application", workers=1, chdir=project, args=["--reload"])

    try:
        assert server.wait_for_log("watching")
        assert version_seen(server) == "one"
        first_pid = server.get("/").body.decode().split(" from ")[1]

        server.mark_log()
        time.sleep(1.1)  # keep the modification time distinct on coarse file systems
        write_app(project, "two")
        assert server.wait_for_log("reloading", timeout=10)
        assert server.wait_for_log("switched to release r2", timeout=30)
        wait_for_version(server, "two")
        assert server.get("/").body.decode().split(" from ")[1] != first_pid

        # A broken edit leaves the previous release serving and is reported.

        server.mark_log()
        time.sleep(1.1)
        (project / "reload_app.py").write_text("def application(:\n")
        assert server.wait_for_log("reload failed", timeout=30)
        assert version_seen(server) == "two"

        # Fixing it reloads again.

        server.mark_log()
        time.sleep(1.1)
        write_app(project, "three")
        assert server.wait_for_log("switched to release r4", timeout=30)
        wait_for_version(server, "three")
    finally:
        server.stop()


def test_files_outside_the_patterns_do_not_reload(project: Path) -> None:
    server = support.start(
        "reload_app:application",
        workers=1,
        chdir=project,
        args=["--reload", "--reload-exclude", "unrelated.py"],
    )

    try:
        assert server.wait_for_log("watching")
        server.mark_log()
        time.sleep(1.1)
        (project / "notes.txt").write_text("not python\n")
        (project / "unrelated.py").write_text("# excluded\n")
        (project / "reload_app.py~").write_text("editor leftover\n")
        assert not server.wait_for_log("reloading", timeout=2)
        assert version_seen(server) == "one"
    finally:
        server.stop()


def test_reload_is_off_by_default(project: Path) -> None:
    server = support.start("reload_app:application", workers=1, chdir=project)

    try:
        assert "watching" not in server.log()
        server.mark_log()
        time.sleep(1.1)
        write_app(project, "two")
        assert not server.wait_for_log("reloading", timeout=2)
        assert version_seen(server) == "one"
    finally:
        server.stop()
