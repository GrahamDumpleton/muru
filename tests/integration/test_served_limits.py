"""Limits a release ships for the requests it serves (design
specification sections 3.3 and 4.6).

A release, a worker group and a mount each set the largest body and the
request timeouts of what they serve. A route's own replace them, and
they replace the virtual host's and the daemon's. A development run
reads the same keys from the project's table, beneath its flags.
"""

from __future__ import annotations

import http.client
import json
import shutil
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon

APPS = support.ROOT / "tests" / "fixtures" / "apps"


@pytest.fixture
def daemon(tmp_path: Path) -> Iterator[Daemon]:
    started = start_daemon(tmp_path)

    try:
        yield started
    finally:
        started.stop()


def apply(daemon: Daemon, path: Path) -> subprocess.CompletedProcess[str]:
    return daemon.muructl("apply", "-f", str(path), check=False)


def post(port: int, path: str, size: int, host: str | None = None) -> int:
    """The status of a POST of `size` bytes."""

    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)

    try:
        headers = {"Host": host} if host else {}
        connection.request("POST", path, body=b"x" * size, headers=headers)
        response = connection.getresponse()
        response.read()
        return response.status
    finally:
        connection.close()


def test_a_release_its_groups_and_its_mounts_set_limits_between_the_hosts_and_a_routes(
    daemon: Daemon, tmp_path: Path
) -> None:
    config = tmp_path / "site.toml"
    config.write_text(
        f"""[app.echo.release.r1]
source = '{APPS}'
python = '{sys.executable}'
entrypoint = 'plain_wsgi:application'
processes = 1
max_body_size = "2KB"
timeouts = {{ request_deadline = "1s" }}

[app.echo.release.r1.groups.slow]
paths = ["/sleep"]
processes = 1
timeouts = {{ request_deadline = "20s" }}

[app.echo.release.r1.mounts.up]
path = "/up"
entrypoint = 'plain_wsgi:application'
max_body_size = "1MB"

[host.site]
names = ["site.test"]
max_body_size = "8KB"
timeouts = {{ request_deadline = "30s", queue_timeout = "3s" }}

[[route]]
host = "site"
app = "echo"

[[route]]
host = "site"
path = "/echo"
app = "echo"
max_body_size = "1MB"
"""
    )
    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr
    daemon.wait_for_routing("echo")
    site = "site.test"

    # The release's body limit stands over its host's, its mount's over
    # the release's, and the route's over them all.

    environ = json.loads(daemon.request("/info", host=site).body)["environ"]
    assert environ["muru.max_body_size"] == 2048
    assert post(daemon.port, "/", 4096, site) == 413
    assert post(daemon.port, "/up/echo", 4096, site) == 200
    assert post(daemon.port, "/echo", 4096, site) == 200

    # So does its deadline: one second for the release, where the host
    # allows thirty, and twenty for the requests its group serves.

    assert daemon.request("/busy?seconds=2", host=site).status == 504
    assert daemon.request("/sleep?seconds=2", host=site).status == 200

    # The effective configuration says where each limit comes from.

    effective = daemon.muructl("config", "show", "--effective").stdout

    for line in [
        "echo@r1: max_body_size = 2KB (the release)",
        "echo@r1: timeouts.request_deadline = 1s (the release)",
        "echo@r1 group slow: timeouts.request_deadline = 20s (the worker group)",
        "echo@r1 mount up: max_body_size = 1MB (the mount)",
        "route site /: max_body_size = 2KB (release echo@r1)",
        "route site /: timeouts.request_deadline = 1s (release echo@r1)",
        "route site /: timeouts.queue_timeout = 3s (host site)",
        "route site /echo: max_body_size = 1MB (the route)",
    ]:
        assert line in effective, f"{line!r} missing from:\n{effective}"

    # The export writes the keys back, and the same file changes nothing.

    exported = daemon.muructl("config", "export").stdout
    assert 'max_body_size = "2KB"' in exported and 'request_deadline = "1s"' in exported
    again = apply(daemon, config)
    assert again.returncode == 0, again.stdout + again.stderr
    assert "deploy" not in again.stdout, again.stdout

    # The keys are checked, and a recorded release is not changed by them.

    config.write_text(config.read_text().replace('max_body_size = "2KB"', 'max_body_size = "4KB"'))
    refused = apply(daemon, config)
    assert refused.returncode != 0
    assert "already exists with different settings (request limits" in (
        refused.stdout + refused.stderr
    )
    config.write_text(
        config.read_text()
        .replace("[app.echo.release.r1", "[app.echo.release.r2")
        .replace('request_deadline = "20s"', 'request_deadline = "0s"')
    )
    refused = apply(daemon, config)
    assert refused.returncode != 0
    assert "worker group slow: timeouts.request_deadline must be more than 0" in (
        refused.stdout + refused.stderr
    )


def test_a_run_reads_the_projects_limits_beneath_its_flags(tmp_path: Path) -> None:
    project = tmp_path / "project"
    project.mkdir()
    shutil.copy(support.FIXTURES / "plain_wsgi.py", project / "app.py")
    (project / "pyproject.toml").write_text(
        "[tool.muru]\nentrypoint = 'app:application'\nmax_body_size = '2KB'\n"
        "timeouts = { request_deadline = '1s' }\n"
    )
    server = support.start(str(project), workers=1, threads=2, chdir=None)

    try:
        environ = json.loads(server.get("/info").body)["environ"]
        assert environ["muru.max_body_size"] == 2048
        assert post(server.port, "/echo", 4096) == 413
        assert server.get("/busy?seconds=2").status == 504
    finally:
        server.stop()

    flagged = support.start(
        str(project),
        workers=1,
        threads=2,
        chdir=None,
        args=["--max-body-size", "1MB", "--timeout", "deadline=20s"],
    )

    try:
        assert post(flagged.port, "/echo", 4096) == 200
        assert flagged.get("/busy?seconds=2").status == 200
    finally:
        flagged.stop()
