"""Per-application users (design specification section 10.9): an
application's user, group and umask are declared and exported, a daemon
that is not root reports them as not applied, and a root daemon runs the
application's host and workers as that user."""

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


@pytest.mark.requires_root
def test_a_root_daemon_runs_the_application_as_its_user() -> None:
    """Run in the Linux virtual machine as root: sudo -n is needed, and the
    interpreter and the source tree must be readable by the user chosen."""

    muructl = support.muructl_path()
    assert muructl is not None
    assert shutil.which("sudo"), "sudo is needed"
    assert subprocess.run(["sudo", "-n", "true"], check=False).returncode == 0, "sudo -n"
    base = Path(tempfile.mkdtemp(prefix="muru-root-", dir="/tmp"))
    os.chmod(base, 0o755)
    port = support.free_port()
    run_dir = base / "run"
    user = "nobody"
    expected = pwd.getpwnam(user)
    env = {**os.environ}
    daemon = subprocess.Popen(
        [
            "sudo",
            "-n",
            "-E",
            str(muructl),
            "daemon",
            "run",
            "--state-dir",
            str(base / "state"),
            "--run-dir",
            str(run_dir),
            "--log-dir",
            str(base / "logs"),
            "--listen",
            f"127.0.0.1:{port}",
        ],
        stdout=(base / "daemon.out").open("ab"),
        stderr=subprocess.STDOUT,
        env=env,
    )

    def muru(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["sudo", "-n", str(muructl), *arguments, "--socket", str(run_dir / "admin.sock")],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )

    try:
        deadline = time.monotonic() + 30

        while time.monotonic() < deadline and muru("status").returncode != 0:
            time.sleep(0.2)

        assert muru("status").returncode == 0, (base / "daemon.out").read_text()
        config = base / "plain.toml"
        config.write_text(
            f"[app.plain]\nuser = '{user}'\numask = '027'\n\n"
            "[app.plain.release.r1]\n"
            f"source = '{PLAIN}'\n"
            "entrypoint = 'plain_wsgi:application'\n"
            f"python = '{sys.executable}'\n"
            "processes = 1\n"
        )
        os.chmod(config, 0o644)
        applied = muru("apply", "-f", str(config))
        assert applied.returncode == 0, (
            applied.stdout + applied.stderr + (base / "daemon.out").read_text()
        )
        info = json.loads(Daemon.request_static(port, "/info").body)
        assert info["uid"] == expected.pw_uid
        assert info["gid"] == expected.pw_gid
        assert info["umask"] == "0o27"
        assert "app.user_not_applied" not in muru("events").stdout
    finally:
        muru("daemon", "stop", "--shutdown")
        daemon.wait(timeout=60)
