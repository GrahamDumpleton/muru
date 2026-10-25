"""Per-application users (design specification section 10.9 and ADR
0026): an application's user, group and umask are declared and exported,
a daemon that is not root reports them as not applied, and a root daemon
runs the edge and every application as the service user unless the
application names a user of its own, warning when that user is root."""

from __future__ import annotations

import json
import os
import pwd
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon

PLAIN = support.FIXTURES


def test_a_declared_user_is_recorded_and_reported_as_not_applied_without_root(
    tmp_path: Path,
) -> None:
    daemon = start_daemon(tmp_path)

    try:
        config = tmp_path / "plain.toml"
        config.write_text(
            "[app.plain]\n"
            "user = 'nobody'\n"
            "umask = '027'\n\n"
            "[app.plain.release.r1]\n"
            f"source = '{PLAIN}'\n"
            "entrypoint = 'plain_wsgi:application'\n"
            f"python = '{sys.executable}'\n"
            "processes = 1\n"
        )
        planned = daemon.muructl("apply", "-f", str(config), "--dry-run", check=False)
        assert "set-identity" in planned.stdout and "user nobody, umask 027" in planned.stdout
        applied = daemon.muructl("apply", "-f", str(config), check=False)
        assert applied.returncode == 0, applied.stdout + applied.stderr
        assert "set-identity    plain                    set" in applied.stdout

        # Without root the host runs as the daemon's user, and says so.

        assert json.loads(daemon.get("/info").body)["uid"] == os.getuid()
        events = daemon.muructl("events").stdout
        assert "app.user_not_applied" in events
        assert "user nobody not applied: the daemon is not root" in events
        listed = daemon.muructl("app", "list").stdout
        assert "nobody umask 027" in listed

        exported = daemon.muructl("config", "export").stdout
        assert "user = 'nobody'" in exported or 'user = "nobody"' in exported
        assert "umask = '027'" in exported or 'umask = "027"' in exported
        assert daemon.muructl("config", "diff", "-f", str(config), check=False).returncode == 0

        # Applying the same again changes nothing; a bad mask is refused.

        again = daemon.muructl("apply", "-f", str(config), check=False)
        assert again.returncode == 0 and "set-identity" not in again.stdout
        config.write_text("[app.plain]\numask = '9z'\n")
        refused = daemon.muructl("apply", "-f", str(config), check=False)
        assert refused.returncode != 0
        assert "umask must be octal digits" in refused.stdout + refused.stderr
    finally:
        daemon.stop()


def test_a_service_user_is_reported_as_not_applied_without_root(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, arguments=["--user", "nobody"])

    try:
        assert daemon.get("/").status == 503
        assert "service user nobody not applied: the daemon is not root" in daemon.log()
    finally:
        daemon.stop()


def root_prerequisites() -> Path:
    """What the root scenarios need: muructl, sudo without a password, and
    a base directory other users can traverse."""

    muructl = support.muructl_path()
    assert muructl is not None
    assert shutil.which("sudo"), "sudo is needed"
    assert subprocess.run(["sudo", "-n", "true"], check=False).returncode == 0, "sudo -n"
    return muructl


class RootDaemon:
    """A daemon run as root through sudo, in the Linux virtual machine: the
    interpreter and the source tree must be readable by the users chosen."""

    def __init__(self, muructl: Path, *arguments: str) -> None:
        self.muructl = muructl
        self.base = Path(tempfile.mkdtemp(prefix="muru-root-", dir="/tmp"))
        os.chmod(self.base, 0o755)
        self.port = support.free_port()
        self.run_dir = self.base / "run"
        self.process = subprocess.Popen(
            [
                "sudo",
                "-n",
                "-E",
                str(muructl),
                "daemon",
                "run",
                "--state-dir",
                str(self.base / "state"),
                "--run-dir",
                str(self.run_dir),
                "--log-dir",
                str(self.base / "logs"),
                "--listen",
                f"127.0.0.1:{self.port}",
                *arguments,
            ],
            stdout=(self.base / "daemon.out").open("ab"),
            stderr=subprocess.STDOUT,
            env={**os.environ},
        )

    def output(self) -> str:
        return (self.base / "daemon.out").read_text()

    def muru(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                "sudo",
                "-n",
                str(self.muructl),
                *arguments,
                "--socket",
                str(self.run_dir / "admin.sock"),
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )

    def wait(self) -> None:
        deadline = time.monotonic() + 30

        while time.monotonic() < deadline and self.muru("status").returncode != 0:
            if self.process.poll() is not None:
                break

            time.sleep(0.2)

        assert self.muru("status").returncode == 0, self.output()

    def apply(self, text: str) -> None:
        config = self.base / "apply.toml"
        config.write_text(text)
        os.chmod(config, 0o644)
        applied = self.muru("apply", "-f", str(config))
        assert applied.returncode == 0, applied.stdout + applied.stderr + self.output()

    def info(self) -> dict[str, Any]:
        return json.loads(Daemon.request_static(self.port, "/info").body)

    def stop(self) -> None:
        if self.process.poll() is None:
            self.muru("daemon", "stop", "--shutdown")
            self.process.wait(timeout=60)


def uid_of(pid: int) -> int:
    """The real uid of a process, from /proc."""

    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith("Uid:"):
            return int(line.split()[1])

    raise AssertionError(f"no Uid line for {pid}")


def release_table(release: str, user: str | None = None) -> str:
    return (
        "[app.plain]\n"
        + (f"user = '{user}'\n" if user else "")
        + f"\n[app.plain.release.{release}]\n"
        f"source = '{PLAIN}'\n"
        "entrypoint = 'plain_wsgi:application'\n"
        f"python = '{sys.executable}'\n"
        "processes = 1\n"
    )


@pytest.mark.requires_root
def test_a_root_daemon_refuses_to_start_without_a_service_user() -> None:
    muructl = root_prerequisites()

    try:
        pwd.getpwnam("muru")
    except KeyError:
        pass
    else:
        pytest.skip("this machine has a muru account, so the default service user exists")

    daemon = RootDaemon(muructl)

    try:
        assert daemon.process.wait(timeout=30) != 0
        output = daemon.output()
        assert "no user named muru" in output
        assert "never as root" in output
        assert "useradd --system --no-create-home muru" in output
        assert "--user" in output and "[daemon]" in output
    finally:
        daemon.stop()

    refused = RootDaemon(muructl, "--user", "root")

    try:
        assert refused.process.wait(timeout=30) != 0
        assert "the service user root is root" in refused.output()
    finally:
        refused.stop()


@pytest.mark.requires_root
def test_a_root_daemon_runs_the_edge_and_applications_as_the_service_user() -> None:
    muructl = root_prerequisites()
    service = pwd.getpwnam("nobody")
    other = pwd.getpwnam("daemon")
    daemon = RootDaemon(muructl, "--user", "nobody")

    try:
        daemon.wait()
        assert "the edge and applications run as the service user" in daemon.output()
        status = daemon.muru("status").stdout
        edge_pid = int(
            next(line.split()[2] for line in status.splitlines() if line.startswith("edge"))
        )
        assert uid_of(edge_pid) == service.pw_uid

        # An application that names no user runs as the service user, and
        # the edge, no longer root, still reaches its sockets.

        daemon.apply(release_table("r1"))
        info = daemon.info()
        assert info["uid"] == service.pw_uid
        assert info["gid"] == service.pw_gid
        assert "app.user_not_applied" not in daemon.muru("events").stdout
        sockets = daemon.run_dir / "apps" / "plain" / "r1" / "r1"
        assert sockets.stat().st_mode & 0o7777 == 0o2750

        # One that names a user runs as that user, with its umask.

        daemon.apply(
            release_table("r2", "daemon").replace("[app.plain]\n", "[app.plain]\numask = '027'\n")
        )
        info = daemon.info()
        assert info["uid"] == other.pw_uid
        assert info["gid"] == other.pw_gid
        assert info["umask"] == "0o27"

        # Root is allowed only by name, and is warned about every time.

        daemon.apply(release_table("r3", "root"))
        assert daemon.info()["uid"] == 0
        events = daemon.muru("events").stdout
        assert "app.user_is_root" in events
        assert "runs as root, which is not recommended" in events
    finally:
        daemon.stop()
