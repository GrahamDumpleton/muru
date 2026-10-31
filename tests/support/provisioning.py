"""Helpers for the provisioning tests: a muru wheel made from the
development checkout, a virtualenv without muru, a locked project, and a
`muructl run` driven until it serves."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import sysconfig
import tempfile
import time
from pathlib import Path

import pytest

from support import server as support

PYTHON_SOURCES = support.ROOT / "packages" / "muru" / "python" / "muru"


def muructl() -> Path:
    path = support.muructl_path()

    if path is None or not support.extension_available():
        pytest.skip("muructl and the extension must be built")

    return path


def uv_available() -> bool:
    return shutil.which("uv") is not None


def muructl_version() -> str:
    """muructl's version as PyPI spells it, from `muructl --version`."""

    completed = subprocess.run(
        [str(muructl()), "--version"], capture_output=True, text=True, check=True
    )
    cargo = completed.stdout.split()[-1]
    match = re.fullmatch(r"(\d+\.\d+\.\d+)(?:-(alpha|beta|rc)\.(\d+))?", cargo)
    assert match, cargo
    release, kind, number = match.groups()

    if kind is None:
        return release

    return release + {"alpha": "a", "beta": "b", "rc": "rc"}[kind] + number


def wheel_tag() -> str:
    """The wheel tag of this interpreter and platform, as maturin names it."""

    major, minor = sys.version_info[:2]
    abi = f"cp{major}{minor}" + ("t" if sysconfig.get_config_var("Py_GIL_DISABLED") else "")
    machine = os.uname().machine

    if sys.platform == "darwin":
        platform = f"macosx_11_0_{'arm64' if machine == 'arm64' else 'x86_64'}"
    else:
        platform = f"manylinux_2_28_{'aarch64' if machine in ('aarch64', 'arm64') else machine}"

    return f"cp{major}{minor}-{abi}-{platform}"


def build_wheel(directory: Path, wheel_version: str) -> Path:
    """A muru wheel for this interpreter in `directory/wheels`, made from
    the development checkout's package (with its built extension) and a
    minimal dist-info, so no network and no maturin are needed."""

    staging = directory / "staging"
    package = staging / "muru"
    shutil.copytree(PYTHON_SOURCES, package, ignore=shutil.ignore_patterns("__pycache__"))
    dist_info = staging / f"muru-{wheel_version}.dist-info"
    dist_info.mkdir()
    (dist_info / "METADATA").write_text(
        f"Metadata-Version: 2.1\nName: muru\nVersion: {wheel_version}\n"
    )
    (dist_info / "WHEEL").write_text(
        "Wheel-Version: 1.0\nGenerator: test\nRoot-Is-Purelib: false\n"
    )
    (dist_info / "RECORD").write_text("")
    wheels = directory / "wheels"
    wheels.mkdir()
    wheel = wheels / f"muru-{wheel_version}-{wheel_tag()}.whl"
    subprocess.run(
        [sys.executable, "-m", "zipfile", "-c", str(wheel), str(package), str(dist_info)],
        check=True,
    )
    return wheels


def bare_venv(directory: Path) -> Path:
    """A virtualenv without pip and without muru, on this interpreter."""

    subprocess.run([sys.executable, "-m", "venv", "--without-pip", str(directory)], check=True)
    return directory / "bin" / "python"


def project(directory: Path, table: str = "") -> Path:
    """A project serving the plain WSGI fixture."""

    directory.mkdir(exist_ok=True)
    shutil.copy(support.FIXTURES / "plain_wsgi.py", directory / "app.py")
    (directory / "pyproject.toml").write_text(
        "[project]\nname = 'hello'\nversion = '0.0.0'\nrequires-python = '>=3.11'\n\n"
        "[tool.muru]\nentrypoint = 'app:application'\n" + table
    )
    return directory


def locked_project(directory: Path, table: str = "") -> Path:
    """A project with a uv.lock made offline, which a dependency-free
    project allows, and no environment."""

    if not uv_available():
        pytest.skip("uv is needed to lock and build")

    project(directory, table)
    subprocess.run(
        ["uv", "lock", "--offline", "--project", str(directory)], check=True, capture_output=True
    )
    return directory


def offline_env(tmp_path: Path, wheels: Path | None = None) -> dict[str, str]:
    """An environment for muructl with the network off limits, its cache in
    the test's directory and, given one, a wheel directory."""

    # CI leaves the repository's own group selection in the environment
    # (UV_NO_GROUP for the free-threaded jobs), which a test project does
    # not define; an empty value is how uv is told to forget it.

    env = {
        "MURU_OFFLINE": "1",
        "MURU_CACHE_DIR": str(tmp_path / "cache"),
        "UV_NO_PROGRESS": "1",
        "UV_NO_GROUP": "",
    }

    if wheels is not None:
        env["MURU_WHEEL_DIR"] = str(wheels)

    return env


def run_until_serving(
    directory: Path,
    env: dict[str, str],
    *args: str,
    python: Path | None = None,
    reload: bool = False,
) -> tuple[subprocess.Popen[bytes], Path]:
    """Start `muructl run` on the project and return the process once it
    serves, with its log; the caller stops it with `stop`."""

    log = Path(tempfile.mkstemp(prefix="muru-provisioning-", suffix=".log")[1])
    command = [str(muructl()), "run", str(directory), "--port", "auto", "--processes", "1", *args]

    if python is not None:
        command += ["--python", str(python)]

    if not reload:
        command.append("--no-reload")

    environment = {key: value for key, value in os.environ.items() if key != "VIRTUAL_ENV"}
    environment.update(env)

    with log.open("wb") as handle:
        process = subprocess.Popen(
            command, stdout=handle, stderr=subprocess.STDOUT, env=environment
        )

    deadline = time.monotonic() + 120

    while time.monotonic() < deadline:
        text = log.read_text(errors="replace")

        if "muru: serving" in text:
            return process, log

        if process.poll() is not None:
            raise RuntimeError(f"muructl exited:\n{text}")

        time.sleep(0.1)

    process.kill()
    raise RuntimeError(f"muructl did not serve:\n{log.read_text(errors='replace')}")


def wait_for(log: Path, text: str, timeout: float = 60.0) -> str:
    """Wait until the log holds `text` and return the log."""

    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        content = log.read_text(errors="replace")

        if text in content:
            return content

        time.sleep(0.1)

    raise AssertionError(f"{text!r} not seen:\n{log.read_text(errors='replace')}")


def stop(process: subprocess.Popen[bytes]) -> None:
    process.terminate()

    try:
        process.wait(timeout=30)
    except subprocess.TimeoutExpired:
        process.kill()
