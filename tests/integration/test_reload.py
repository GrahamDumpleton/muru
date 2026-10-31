"""Reloading under `muructl run`, on by default in development mode: a
changed source file starts a new release (sections 9.8 and 13.1)."""

from __future__ import annotations

import os
import re
import shutil
import textwrap
import time
from pathlib import Path

import pytest
from support import server as support

APP = """
import os

import muru

VERSION = {version!r}


def application(environ, start_response):
    body = f"{{VERSION}} from {{os.getpid()}} in {{muru.context().release}}".encode()
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", str(len(body)))])
    return [body]
"""


def write_app(directory: Path, version: str) -> None:
    """Write the application at `version`, with a modification time a whole
    second past the file's last one.

    Python accepts cached bytecode whose recorded source mtime, in whole
    seconds, and size match the file, and every version here is the same
    size, so a rewrite within the second of the last one would be served
    from the cache of the version before.
    """

    path = directory / "reload_app.py"

    try:
        previous = int(path.stat().st_mtime)
    except FileNotFoundError:
        previous = None

    path.write_text(textwrap.dedent(APP.format(version=version)))

    if previous is not None and int(path.stat().st_mtime) <= previous:
        os.utime(path, (previous + 1, previous + 1))


def version_seen(server: support.Server) -> str:
    return server.get("/").body.decode().split(" from ")[0]


def pid_seen(server: support.Server) -> str:
    return server.get("/").body.decode().split(" from ")[1].split(" in ")[0]


def release_seen(server: support.Server) -> str:
    """The release the application reports through muru.context()."""

    return server.get("/").body.decode().split(" in ")[1]


def release_switched_to(server: support.Server) -> str:
    """The release the last `switched to release` line of the log names."""

    return re.findall(r"switched to release (r\d+)", server.log())[-1]


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

    # Release numbers are not checked: a change the watcher sees in two
    # polls abandons the start it interrupts, which takes a number, as
    # section 9.8 allows. The content served is what counts.

    try:
        assert server.wait_for_log("watching")
        assert version_seen(server) == "one"
        assert release_seen(server) == "r1"
        first_pid = pid_seen(server)

        server.mark_log()
        time.sleep(1.1)  # keep the modification time distinct on coarse file systems
        write_app(project, "two")
        assert server.wait_for_log("reloading", timeout=10)
        assert server.wait_for_log("switched to release r", timeout=30)
        wait_for_version(server, "two")
        assert pid_seen(server) != first_pid

        # muru.context().release names the release the log does.

        assert release_seen(server) == release_switched_to(server)

        # A broken edit leaves the previous release serving and is reported.

        server.mark_log()
        time.sleep(1.1)
        (project / "reload_app.py").write_text("def application(:\n")
        assert server.wait_for_log("failed to start", timeout=30)
        assert version_seen(server) == "two"

        # The failure line names the attempt and the release that keeps
        # serving; the attempt keeps its number, as section 9.8 says.

        line = re.search(
            r"muru: release (r\d+) failed to start: .*; release (r\d+) keeps serving", server.log()
        )
        assert line is not None, server.log()
        assert line.group(2) == release_seen(server)
        assert int(line.group(1)[1:]) > int(line.group(2)[1:])

        # Fixing it reloads again.

        server.mark_log()
        time.sleep(1.1)
        write_app(project, "three")
        assert server.wait_for_log("switched to release r", timeout=30)
        wait_for_version(server, "three")
        assert release_seen(server) == release_switched_to(server)
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


def test_reload_is_on_by_default_and_turned_off_by_flag_or_file(project: Path) -> None:
    server = support.start("reload_app:application", workers=1, chdir=project, reload=None)

    # No flag: the run watches, says so, and reloads.

    try:
        assert server.wait_for_log("the run reloads on change")
        assert server.wait_for_log("watching")
        server.mark_log()
        time.sleep(1.1)
        write_app(project, "two")
        assert server.wait_for_log("reloading", timeout=10)
        wait_for_version(server, "two")
    finally:
        server.stop()

    # --no-reload turns it off for the run.

    write_app(project, "one")
    server = support.start("reload_app:application", workers=1, chdir=project, reload=False)

    try:
        assert server.wait_for_log("development mode")
        assert "watching" not in server.log()
        assert "reloads on change" not in server.log()
        server.mark_log()
        time.sleep(1.1)
        write_app(project, "two")
        assert not server.wait_for_log("reloading", timeout=2)
        assert version_seen(server) == "one"
    finally:
        server.stop()

    # reload = false in the project's run table turns it off for good,
    # and --reload turns it back on over the file.

    write_app(project, "one")
    (project / "muru.toml").write_text(
        "entrypoint = 'reload_app:application'\n[run]\nreload = false\n"
    )
    server = support.start("reload_app:application", workers=1, chdir=project, reload=None)

    try:
        assert server.wait_for_log("development mode")
        assert "watching" not in server.log()
    finally:
        server.stop()

    server = support.start("reload_app:application", workers=1, chdir=project, reload=True)

    try:
        assert server.wait_for_log("watching")
    finally:
        server.stop()


def test_included_patterns_add_to_the_default(project: Path) -> None:
    (project / "page.html").write_text("<p>one</p>\n")
    server = support.start(
        "reload_app:application",
        workers=1,
        chdir=project,
        args=["--reload", "--reload-include", "**/*.html", "--reload-exclude", "unrelated.py"],
    )

    try:
        # The start-up line names the patterns in force: the default, the
        # include added to it, and the exclude.

        assert server.wait_for_log("for changes to **/*.py, **/*.html, except unrelated.py")

        # A template change reloads, and so does a module change: the
        # include added **/*.html without replacing **/*.py.

        server.mark_log()
        time.sleep(1.1)
        (project / "page.html").write_text("<p>two</p>\n")
        assert server.wait_for_log("page.html changed", timeout=10)
        assert server.wait_for_log("switched to release r", timeout=30)

        server.mark_log()
        time.sleep(1.1)
        write_app(project, "two")
        assert server.wait_for_log("reload_app.py changed", timeout=10)
        wait_for_version(server, "two")
    finally:
        server.stop()
