"""Mounts (design specification section 6.7).

A release serves several entrypoints from the same workers: the release's
own at / and named mounts at their paths, any mix of WSGI and ASGI. The
longest mount path wins, the mount's path is SCRIPT_NAME (root_path) and
the rest is PATH_INFO (path including root_path for ASGI), an ASGI mount
beside a WSGI default gets the worker's event loop and its own lifespan,
and WebSockets reach an ASGI mount. Mounts come from the project's table
in development mode and from the table or the applied file under the
daemon. A worker group that lists mounts serves those and the default
mount, and its paths must lie within them (ADR 0033).
"""

from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon

MOUNTED = support.ROOT / "tests" / "fixtures" / "projects" / "mounted"
APPS = support.ROOT / "tests" / "fixtures" / "apps"


def report(response: support.Response) -> dict[str, Any]:
    assert response.status == 200, response.body
    details: dict[str, Any] = json.loads(response.body)
    return details


@pytest.fixture(scope="module")
def development() -> Iterator[support.Server]:
    instance = support.start(str(MOUNTED), workers=None, threads=None, chdir=None)

    try:
        yield instance
    finally:
        instance.stop()


def test_paths_reach_their_mounts_with_the_right_prefixes(development: support.Server) -> None:
    root = report(development.get("/anything"))
    assert (root["mount"], root["script_name"], root["path_info"]) == ("default", "", "/anything")

    reports = report(development.get("/reports/daily"))
    assert (reports["mount"], reports["script_name"], reports["path_info"]) == (
        "reports",
        "/reports",
        "/daily",
    )
    assert report(development.get("/reports"))["path_info"] == ""
    assert report(development.get("/reportsx"))["mount"] == "default", "segment boundary"

    # The ASGI mount runs on the worker's loop beside the WSGI default,
    # with lifespan state of its own, and sees the full path.

    asgi = report(development.get("/asgi/info"))
    assert (asgi["mount"], asgi["root_path"], asgi["path"]) == ("asgi", "/asgi", "/asgi/info")
    assert asgi["started_in"] == asgi["pid"]
    assert root["pid"] == asgi["pid"], "one worker serves every mount"
    assert root["context_interface"] == "wsgi", "the context names the default mount's interface"

    # ADR 0033: the reports group's worker serves the reports mount and
    # the default mount, and nothing else.

    assert reports["pid"] != root["pid"], "the reports group has a worker of its own"
    log = development.log()
    assert "worker 1 serves mount reports" in log or "mount reports: mounted:reports_app" in log
    assert "muru: group reports serves /reports/* serving mount reports with 1 worker" in log


def test_websockets_reach_an_asgi_mount(development: support.Server) -> None:
    websockets = pytest.importorskip("websockets.sync.client")

    with websockets.connect(f"ws://127.0.0.1:{development.port}/asgi/ws") as socket:
        socket.send("hello")
        assert socket.recv() == "echo:hello"


def test_the_start_up_output_names_each_mount(development: support.Server) -> None:
    log = development.log()

    # The host says what each entrypoint is served as, and the serving
    # lines say where each mount is and that the workers have a loop.

    assert "mounted:wsgi_app is served as WSGI" in log
    assert "mount asgi: mounted:asgi_app is served as ASGI" in log
    assert "mount reports: mounted:reports_app is served as WSGI" in log
    assert "of 2 threads and an event loop of concurrency 1000" in log
    assert "muru: mount asgi serves /asgi from mounted:asgi_app" in log
    assert "muru: mount reports serves /reports from mounted:reports_app" in log


def test_print_config_writes_the_mounts() -> None:
    muructl = support.muructl_path()
    assert muructl is not None
    completed = subprocess.run(
        [str(muructl), "run", str(MOUNTED), "--print-config", "--python", sys.executable],
        capture_output=True,
        text=True,
        check=True,
        timeout=30,
    )

    release = tomllib.loads(completed.stdout)["app"]["mounted"]["release"]["r1"]
    assert release["mounts"]["asgi"] == {"path": "/asgi", "entrypoint": "mounted:asgi_app"}
    assert release["mounts"]["reports"]["interface"] == "wsgi"


@pytest.fixture
def daemon(tmp_path: Path) -> Iterator[Daemon]:
    instance = start_daemon(tmp_path)

    try:
        yield instance
    finally:
        instance.stop()


def test_mounts_under_the_daemon_from_the_table_and_the_file(
    daemon: Daemon, tmp_path: Path
) -> None:
    assert daemon.deploy("mounted", MOUNTED) == "muru: release mounted@r1 is ready and serving"
    assert report(daemon.request("/asgi/info"))["mount"] == "asgi"
    assert report(daemon.request("/reports/x"))["script_name"] == "/reports"

    # release show lists the mounts with what the host detected each as,
    # and the release's log has the host's own lines.

    shown = [
        " ".join(line.split())
        for line in daemon.muructl("release", "show", "mounted@r1").stdout.splitlines()
    ]
    assert "state ready, serving" in shown
    assert "entrypoint mounted:wsgi_app, WSGI (detected)" in shown
    assert "mount asgi /asgi from mounted:asgi_app, ASGI (detected)" in shown
    assert "mount reports /reports from mounted:reports_app, WSGI" in shown
    assert any(
        line.startswith("group reports /reports/* serving mount reports: 1 process of 2 threads")
        for line in shown
    ), shown
    assert report(daemon.request("/reports/x"))["pid"] != report(daemon.request("/"))["pid"]
    assert any(
        line.startswith("workers 1 process of 2 threads and an event loop") for line in shown
    )
    logs = daemon.muructl("logs", "mounted").stdout
    assert "mount asgi: mounted:asgi_app is served as ASGI" in logs

    # A declared release's mounts replace the table's; the export writes
    # them whole and applies back unchanged.

    config = tmp_path / "echo.toml"
    config.write_text(
        f"""[app.echo.release.r1]
source = '{APPS}'
entrypoint = 'plain_wsgi:application'
python = '{sys.executable}'

[app.echo.release.r1.mounts.asgi]
path = "/asgi/"
entrypoint = "plain_asgi:application"
interface = "asgi"
"""
    )
    applied = daemon.muructl("apply", "-f", str(config))
    assert "ready and serving" in applied.stdout, applied.stdout
    environ = json.loads(daemon.request("/info").body)["environ"]
    assert environ["SCRIPT_NAME"] == "" and environ["PATH_INFO"] == "/info"
    scope = json.loads(daemon.request("/asgi/info").body)["scope"]
    assert (scope["root_path"], scope["path"]) == ("/asgi", "/asgi/info")

    exported = tomllib.loads(daemon.muructl("config", "export", "--app", "echo").stdout)
    assert exported["app"]["echo"]["release"]["r1"]["mounts"] == {
        "asgi": {"path": "/asgi", "entrypoint": "plain_asgi:application", "interface": "asgi"}
    }
    assert "no changes" in daemon.muructl("apply", "-f", str(config)).stdout

    config.write_text(config.read_text().replace('path = "/asgi/"', 'path = "/async"'))
    refused = daemon.muructl("apply", "-f", str(config), check=False)
    assert refused.returncode == 1
    assert "mounts asgi plain_asgi:application at /async, recorded asgi" in refused.stdout


def test_a_group_must_serve_the_mounts_its_paths_reach(daemon: Daemon, tmp_path: Path) -> None:
    # ADR 0033: a group that lists mounts may not claim a path that
    # reaches a mount it leaves out, and must name mounts that exist.

    config = tmp_path / "echo.toml"
    head = f"""[app.echo.release.r1]
source = '{APPS}'
entrypoint = 'plain_wsgi:application'
python = '{sys.executable}'

[app.echo.release.r1.mounts.asgi]
path = "/asgi"
entrypoint = "plain_asgi:application"
interface = "asgi"

[app.echo.release.r1.mounts.other]
path = "/other"
entrypoint = "plain_wsgi:application"
"""

    for table, expected in [
        (
            "[app.echo.release.r1.groups.hot]\npaths = ['/asgi/*']\nmounts = ['other']\n",
            "worker group hot: its path /asgi reaches mount asgi at /asgi, which it does not serve",
        ),
        (
            "[app.echo.release.r1.groups.hot]\npaths = ['/']\nmounts = ['other']\n",
            "worker group hot: its path / reaches mount asgi at /asgi, which it does not serve",
        ),
        (
            "[app.echo.release.r1.groups.hot]\npaths = ['/x']\nmounts = ['nope']\n",
            "worker group hot: no mount named nope",
        ),
    ]:
        config.write_text(head + table)
        refused = daemon.muructl("apply", "-f", str(config), check=False)
        assert refused.returncode == 1, refused.stdout
        assert expected in refused.stdout, refused.stdout

    # A group whose paths lie within its mounts is accepted, and the
    # export writes the subset.

    config.write_text(
        head + "[app.echo.release.r1.groups.hot]\npaths = ['/other/*']\nmounts = ['other']\n"
    )
    applied = daemon.muructl("apply", "-f", str(config))
    assert "ready and serving" in applied.stdout, applied.stdout
    assert daemon.request("/other/info").status == 200
    exported = tomllib.loads(daemon.muructl("config", "export", "--app", "echo").stdout)
    assert exported["app"]["echo"]["release"]["r1"]["groups"]["hot"]["mounts"] == ["other"]


def test_invalid_mounts_are_refused(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "shop.toml"

    for table, expected in [
        (
            "[app.shop.release.r1.mounts.Bad]\npath = '/x'\nentrypoint = 'a:b'\n",
            "not a valid mount name",
        ),
        (
            "[app.shop.release.r1.mounts.root]\npath = '/'\nentrypoint = 'a:b'\n",
            "is not a path below /",
        ),
        (
            "[app.shop.release.r1.mounts.x]\npath = 'x'\nentrypoint = 'a:b'\n",
            "is not a path below /",
        ),
        ("[app.shop.release.r1.mounts.x]\npath = '/x'\nentrypoint = 'ab'\n", "module:callable"),
        (
            "[app.shop.release.r1.mounts.x]\npath = '/x'\nentrypoint = 'a:b'\n"
            "[app.shop.release.r1.mounts.y]\npath = '/x/'\nentrypoint = 'a:c'\n",
            "another mount has the path /x",
        ),
        (
            "[app.shop.release.r1.mounts.x]\npath = '/x'\nentrypoint = 'a:b'\n"
            "interface = 'asgi'\nthreads = 2\n",
            "an ASGI mount runs on the event loop and has no threads",
        ),
        (
            "[app.shop.release.r1.mounts.x]\npath = '/x'\nentrypoint = 'a:b'\n"
            "threads = { reserved = 3, max = 2 }\n",
            "max 2 must be at least 1 and no less than reserved 3",
        ),
    ]:
        config.write_text(
            f"[app.shop.release.r1]\nsource = '{SHOP}'\npython = '{sys.executable}'\n\n{table}"
        )
        refused = daemon.muructl("apply", "-f", str(config), check=False)
        assert refused.returncode == 1, refused.stdout
        assert expected in refused.stdout, refused.stdout

    assert daemon.releases() == []
