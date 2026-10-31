"""Environment builds in development (design specification section 8.2):
`muructl run` building a missing .venv from uv.lock under the build mode,
leaving an existing one alone and saying when it is behind the lock,
building again when the lock changes under reload, and `muructl build` as
the same procedure on its own."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from support import provisioning as prov


def wheels_for(tmp_path: Path) -> Path:
    return prov.build_wheel(tmp_path, prov.muructl_version())


def test_a_project_with_a_lock_and_no_environment_is_built_once(tmp_path: Path) -> None:
    directory = prov.locked_project(tmp_path / "project")
    env = prov.offline_env(tmp_path, wheels_for(tmp_path))
    process, log = prov.run_until_serving(directory, env, python=Path(sys.executable))

    try:
        text = log.read_text()
        assert "muru: no environment for this project, building .venv with uv sync --frozen" in text
        assert (directory / ".venv" / "bin" / "python").is_file()
    finally:
        prov.stop(process)

    # The second run finds the environment and builds nothing.

    process, log = prov.run_until_serving(directory, env)

    try:
        text = log.read_text()
        assert "building .venv" not in text
        assert "behind uv.lock" not in text
    finally:
        prov.stop(process)


def test_never_and_allow_public_do_not_build(tmp_path: Path) -> None:
    directory = prov.locked_project(tmp_path / "project")
    env = prov.offline_env(tmp_path, wheels_for(tmp_path))

    completed = subprocess.run(
        [
            str(prov.muructl()),
            "run",
            str(directory),
            "--build",
            "never",
            "--print-config",
            "--python",
            sys.executable,
        ],
        capture_output=True,
        text=True,
        timeout=60,
        env={**os.environ, **env},
    )
    assert completed.returncode == 0, completed.stderr
    assert not (directory / ".venv").exists()

    # Under --allow-public the first-run build is off, and the line names
    # the command that builds.

    process, log = prov.run_until_serving(
        directory, env, "--allow-public", "--bind", "127.0.0.1", python=Path(sys.executable)
    )

    try:
        text = log.read_text()
        assert "the build mode is never; muructl build makes one from uv.lock" in text
        assert not (directory / ".venv").exists()
    finally:
        prov.stop(process)


def test_muructl_build_makes_the_environment_and_run_uses_it(tmp_path: Path) -> None:
    directory = prov.locked_project(tmp_path / "project")
    env = prov.offline_env(tmp_path, wheels_for(tmp_path))
    completed = subprocess.run(
        [str(prov.muructl()), "build", str(directory), "--python", sys.executable],
        capture_output=True,
        text=True,
        timeout=120,
        env={**os.environ, **env},
    )

    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert f"muru: building {directory / '.venv'} with uv sync --frozen" in completed.stdout
    assert f"muru: built {directory / '.venv'} on Python" in completed.stdout
    assert "muru: runtime library" in completed.stdout
    assert (directory / ".venv" / "bin" / "python").is_file()

    process, log = prov.run_until_serving(directory, env)

    try:
        assert "building .venv" not in log.read_text()
    finally:
        prov.stop(process)


def test_an_environment_behind_the_lock_is_said_and_built_on_request(tmp_path: Path) -> None:
    directory = prov.locked_project(tmp_path / "project")
    env = prov.offline_env(tmp_path, wheels_for(tmp_path))
    subprocess.run(
        [str(prov.muructl()), "build", str(directory), "--python", sys.executable],
        check=True,
        capture_output=True,
        env={**os.environ, **env},
    )

    # Something the lock does not know about, installed by hand: the
    # environment is now behind the lock, which uv's dry run reports.

    site = next((directory / ".venv" / "lib").glob("python*/site-packages"))
    (site / "stray").mkdir()
    (site / "stray" / "__init__.py").write_text("")
    (site / "stray-1.0.dist-info").mkdir()
    (site / "stray-1.0.dist-info" / "METADATA").write_text(
        "Metadata-Version: 2.1\nName: stray\nVersion: 1.0\n"
    )
    (site / "stray-1.0.dist-info" / "RECORD").write_text(
        "stray/__init__.py,,\nstray-1.0.dist-info/METADATA,,\nstray-1.0.dist-info/RECORD,,\n"
    )

    process, log = prov.run_until_serving(directory, env)

    try:
        text = log.read_text()
        assert "muru: the environment is behind uv.lock (would uninstall 1 package)" in text
        assert (site / "stray").is_dir(), "auto leaves an existing environment alone"
    finally:
        prov.stop(process)

    process, log = prov.run_until_serving(directory, env, "--build")

    try:
        text = log.read_text()
        assert "muru: the build mode is always, building .venv with uv sync --frozen" in text
        assert not (site / "stray-1.0.dist-info").exists(), (
            "a bare --build brings the environment up to date"
        )
    finally:
        prov.stop(process)


def test_a_lock_change_under_reload_builds_before_the_new_release(tmp_path: Path) -> None:
    directory = prov.locked_project(tmp_path / "project")
    env = prov.offline_env(tmp_path, wheels_for(tmp_path))
    process, log = prov.run_until_serving(directory, env, python=Path(sys.executable), reload=True)

    try:
        # The watcher starts after the first release serves, so a change
        # made before "watching" is printed would go unseen.

        prov.wait_for(log, "muru: watching")
        lock = directory / "uv.lock"
        lock.write_text(lock.read_text() + "\n# touched by the test\n")
        text = prov.wait_for(log, "muru: switched to release r2")
        assert "muru: uv.lock changed, building .venv with uv sync --frozen" in text

        # The manifest alone reloads without a build, and is said to be
        # newer than the lock.

        manifest = directory / "pyproject.toml"
        manifest.write_text(manifest.read_text() + "\n# touched by the test\n")
        text = prov.wait_for(log, "muru: switched to release r3")
        assert text.count("building .venv") == 2, (
            "the first run and the lock change, not the manifest"
        )
        assert "pyproject.toml changed and is newer than uv.lock; run uv lock" in text
    finally:
        prov.stop(process)
