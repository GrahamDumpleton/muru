"""Environment builds under the daemon (design specification section 8.2):
a source with a lock and no .venv gets an environment of its own in the
state directory, built before the release starts and recorded with it; the
build mode and a failed build."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest
from support import provisioning as prov
from support.daemon import Daemon, start_daemon


@pytest.fixture
def offline_daemon(tmp_path: Path):
    """A daemon with the network off limits and a wheel directory holding
    the runtime library for this interpreter."""

    wheels = prov.build_wheel(tmp_path, prov.muructl_version())
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(
        tmp_path / "daemon",
        config_extra=f"\n[tools]\noffline = true\nwheel_dir = '{wheels}'\n",
        env={"MURU_OFFLINE": "1"},
    )

    try:
        yield daemon
    finally:
        daemon.stop()


def test_a_source_with_a_lock_and_no_venv_is_built_before_it_starts(
    offline_daemon: Daemon, tmp_path: Path
) -> None:
    daemon = offline_daemon
    source = prov.locked_project(tmp_path / "hello")
    environment = tmp_path / "daemon" / "state" / "envs" / "hello" / "r1"

    assert daemon.deploy("hello", source) == "muru: release hello@r1 is ready and serving"
    assert daemon.get("/").status == 200
    assert (environment / "bin" / "python").is_file()
    assert not (source / ".venv").exists(), "the source is left alone"

    shown = daemon.muructl("release", "show", "hello@r1").stdout
    assert f"environment  {environment} (built, build.when = auto)" in shown
    python_line = next(line for line in shown.splitlines() if line.startswith("python"))
    assert f"(3.{sys.version_info[1]}." in python_line, "the interpreter the build used, probed"

    events = [kind for kind in daemon.events() if kind.startswith("release.")]
    assert (
        events.index("release.building")
        < events.index("release.built")
        < events.index("release.ready")
    )

    log = (tmp_path / "daemon" / "logs" / "apps" / "hello" / "r1.log").read_text()
    assert "[build] uv sync --frozen --python " in log
    assert "--no-dev --no-build --compile-bytecode" in log


def test_never_runs_on_what_is_there(offline_daemon: Daemon, tmp_path: Path) -> None:
    daemon = offline_daemon
    source = prov.locked_project(tmp_path / "hello")

    assert (
        daemon.deploy("hello", source, "--build", "never")
        == "muru: release hello@r1 is ready and serving"
    )
    assert not (tmp_path / "daemon" / "state" / "envs").exists()

    shown = daemon.muructl("release", "show", "hello@r1").stdout
    assert "environment" not in shown
    assert f"python       {sys.executable}" in shown or "python       " in shown


def test_a_broken_lock_fails_the_release_with_the_build_output(
    offline_daemon: Daemon, tmp_path: Path
) -> None:
    daemon = offline_daemon
    source = prov.locked_project(tmp_path / "hello")
    (source / "uv.lock").write_text("this is not a lock\n")

    completed = daemon.muructl(
        "deploy", "hello", str(source), "--python", sys.executable, check=False
    )
    assert completed.returncode != 0
    assert "failed" in (completed.stdout + completed.stderr).lower()

    releases = daemon.releases("hello")
    assert [(release["release"], release["state"]) for release in releases] == [
        ("hello@r1", "failed")
    ]
    assert "release.build_failed" in daemon.events()

    log = (tmp_path / "daemon" / "logs" / "apps" / "hello" / "r1.log").read_text()
    assert "[build]" in log
