"""Interpreter resolution (design specification section 8.1): a path or a
version request, the project's own requests from `[tool.muru]`,
`.python-version` and `requires-python`, the probe that refuses an
interpreter Muru cannot run on or one outside the requirement, and what
the daemon records about the interpreter a release runs on."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture

VERSION = f"{sys.version_info[0]}.{sys.version_info[1]}"


def muructl() -> Path:
    path = support.muructl_path()

    if path is None or not support.extension_available():
        pytest.skip("muructl and the extension must be built")

    return path


def uv_available() -> bool:
    return shutil.which("uv") is not None


def project(directory: Path, requires: str | None = None, extra: str = "") -> Path:
    """A project serving the plain WSGI fixture, with a pyproject.toml
    whose [project] table may carry requires-python."""

    directory.mkdir(exist_ok=True)
    shutil.copy(support.FIXTURES / "plain_wsgi.py", directory / "app.py")
    project_table = "[project]\nname = 'hello'\nversion = '0.0.0'\n"

    if requires is not None:
        project_table += f"requires-python = '{requires}'\n"

    (directory / "pyproject.toml").write_text(
        project_table + "\n[tool.muru]\nentrypoint = 'app:application'\n" + extra
    )

    return directory


def run_once(
    directory: Path, *args: str, env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    """Run `muructl run --print-config`, which resolves the interpreter and
    exits, with the active virtualenv hidden so the project decides."""

    environment = {key: value for key, value in os.environ.items() if key != "VIRTUAL_ENV"}
    environment.update(env or {})

    return subprocess.run(
        [str(muructl()), "run", str(directory), "--print-config", *args],
        capture_output=True,
        text=True,
        timeout=60,
        env=environment,
    )


def test_a_version_request_on_the_command_line_is_resolved_with_uv(tmp_path: Path) -> None:
    if not uv_available():
        pytest.skip("uv is needed to resolve a version request")

    directory = project(tmp_path / "project")
    completed = run_once(directory, "--python", VERSION)

    assert completed.returncode == 0, completed.stderr
    assert "python = " in completed.stdout

    # The interpreter written is the version asked for, as the probe saw it.

    written = next(line for line in completed.stdout.splitlines() if line.startswith("python = "))
    python = written.split("=", 1)[1].strip().strip("'\"")
    probed = subprocess.run(
        [python, "-c", "import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')"],
        capture_output=True,
        text=True,
        check=True,
    )
    assert probed.stdout.strip() == VERSION


def test_the_project_version_file_names_the_interpreter(tmp_path: Path) -> None:
    if not uv_available():
        pytest.skip("uv is needed to resolve a version request")

    directory = project(tmp_path / "project")
    (directory / ".python-version").write_text(f"{VERSION}\n")
    completed = run_once(directory)

    assert completed.returncode == 0, completed.stderr
    assert "python = " in completed.stdout


def test_requires_python_the_interpreter_does_not_meet_fails_before_anything_starts(
    tmp_path: Path,
) -> None:
    directory = project(tmp_path / "project", requires=">=3.99")
    completed = run_once(directory, "--python", sys.executable)

    assert completed.returncode != 0
    assert "but the project requires >=3.99" in completed.stderr
    assert sys.executable.rsplit("/", 1)[-1] in completed.stderr or "python" in completed.stderr


def test_a_request_outside_requires_python_is_refused(tmp_path: Path) -> None:
    directory = project(tmp_path / "project", requires=">=3.12")
    completed = run_once(directory, "--python", "3.11")

    assert completed.returncode != 0
    assert "Python 3.11 (from --python) is outside the project's requires-python >=3.12" in (
        completed.stderr
    )


def test_a_request_the_machine_cannot_satisfy_names_the_install_command(tmp_path: Path) -> None:
    if not uv_available():
        pytest.skip("uv is needed to resolve a version request")

    directory = project(tmp_path / "project")
    completed = run_once(directory, "--python", "3.99", env={"UV_PYTHON_DOWNLOADS": "never"})

    assert completed.returncode != 0
    assert "no Python matching 3.99 (from --python) was found on this machine" in completed.stderr
    assert "uv python install 3.99" in completed.stderr


def test_a_request_without_uv_says_uv_is_needed(tmp_path: Path) -> None:
    directory = project(tmp_path / "project")

    # A PATH with nothing on it but the test interpreter's directory, and
    # MURU_UV pointing nowhere, so no uv is found.

    completed = run_once(
        directory,
        "--python",
        "3.13",
        env={"PATH": str(Path(sys.executable).parent), "MURU_UV": ""},
    )

    assert completed.returncode != 0
    assert "needs uv to find it, and uv was not found" in completed.stderr


def test_the_daemon_records_what_the_release_runs_on(daemon: Daemon) -> None:
    from support.daemon import SHOP

    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    shown = daemon.muructl("release", "show", "shop@r1").stdout

    python_line = next(line for line in shown.splitlines() if line.startswith("python"))
    assert f"({sys.version_info[0]}.{sys.version_info[1]}." in python_line
    assert f"cp{sys.version_info[0]}{sys.version_info[1]}" in python_line


def test_the_daemon_resolves_a_version_request(daemon: Daemon, tmp_path: Path) -> None:
    if not uv_available():
        pytest.skip("uv is needed to resolve a version request")

    from support.daemon import SHOP

    # The test interpreter's muru has to be importable by the interpreter
    # uv finds for the request, which is so when the request names the
    # test interpreter's own version and uv finds its base interpreter
    # with the runtime library installed; otherwise the release fails at
    # import, which is still a resolution that worked. Either way the
    # request is recorded.

    completed = daemon.muructl(
        "deploy", "shop", str(SHOP), "--python", VERSION, "--no-wait", check=False
    )
    assert completed.returncode == 0, completed.stdout + completed.stderr
    shown = daemon.muructl("release", "show", "shop@r1").stdout
    python_line = next(line for line in shown.splitlines() if line.startswith("python"))
    assert f"requested {VERSION} from --python" in python_line
