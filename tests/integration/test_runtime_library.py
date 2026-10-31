"""The runtime library provided by muructl (design specification section
8.3): the environment's own `muru` when it matches muructl's version, else
an unpacked wheel on the host's PYTHONPATH, fetched from a wheel directory
when the network is off limits, and `muructl runtime install` for images."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import sysconfig
import tempfile
import time
from importlib.metadata import version
from pathlib import Path

import pytest
from support import server as support

PYTHON_SOURCES = support.ROOT / "packages" / "muru" / "python" / "muru"


def muructl() -> Path:
    path = support.muructl_path()

    if path is None or not support.extension_available():
        pytest.skip("muructl and the extension must be built")

    return path


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
    """A muru wheel for this interpreter, made from the development
    checkout's package (with its built extension) and a minimal dist-info,
    so no network and no maturin are needed."""

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


def project(directory: Path) -> Path:
    directory.mkdir(exist_ok=True)
    shutil.copy(support.FIXTURES / "plain_wsgi.py", directory / "app.py")
    (directory / "pyproject.toml").write_text("[tool.muru]\nentrypoint = 'app:application'\n")
    return directory


def run_until_serving(
    directory: Path, python: Path, env: dict[str, str]
) -> tuple[subprocess.Popen[bytes], str]:
    """Start muructl run and return the process once it serves, with its
    output so far; the caller stops it."""

    log = Path(tempfile.mkstemp(prefix="muru-runtime-", suffix=".log")[1])

    with log.open("wb") as handle:
        process = subprocess.Popen(
            [
                str(muructl()),
                "run",
                str(directory),
                "--python",
                str(python),
                "--port",
                "auto",
                "--no-reload",
                "--processes",
                "1",
            ],
            stdout=handle,
            stderr=subprocess.STDOUT,
            env={**os.environ, **env},
        )

    deadline = time.monotonic() + 60

    while time.monotonic() < deadline:
        text = log.read_text(errors="replace")

        if "muru: serving" in text:
            return process, text

        if process.poll() is not None:
            raise RuntimeError(f"muructl exited:\n{text}")

        time.sleep(0.1)

    process.kill()
    raise RuntimeError(f"muructl did not serve:\n{log.read_text(errors='replace')}")


def stop(process: subprocess.Popen[bytes]) -> None:
    process.terminate()

    try:
        process.wait(timeout=30)
    except subprocess.TimeoutExpired:
        process.kill()


def test_a_matching_environment_copy_is_used_and_nothing_fetched(tmp_path: Path) -> None:
    if version("muru") != muructl_version():
        pytest.skip("the development virtualenv's muru does not match muructl (run just dev)")

    process, text = run_until_serving(
        project(tmp_path / "project"),
        Path(sys.executable),
        {"MURU_OFFLINE": "1", "MURU_CACHE_DIR": str(tmp_path / "cache")},
    )

    try:
        assert f"muru: runtime library {version('muru')} from the environment" in text
        assert "fetched the runtime library" not in text
    finally:
        stop(process)

    assert not (tmp_path / "cache" / "runtime").exists()


def test_an_environment_without_muru_gets_the_library_from_the_wheel_directory(
    tmp_path: Path,
) -> None:
    wanted = muructl_version()
    wheels = build_wheel(tmp_path, wanted)
    python = bare_venv(tmp_path / "venv")
    cache = tmp_path / "cache"
    process, text = run_until_serving(
        project(tmp_path / "project"),
        python,
        {"MURU_OFFLINE": "1", "MURU_WHEEL_DIR": str(wheels), "MURU_CACHE_DIR": str(cache)},
    )

    try:
        assert f"muru: fetched the runtime library muru-{wanted}-" in text
        unpacked = cache / "runtime" / wanted
        assert f"muru: runtime library {wanted} from {unpacked}" in text
        assert any(
            (unpacked / tag / "muru" / "__init__.py").is_file() for tag in os.listdir(unpacked)
        )
    finally:
        stop(process)

    # The second run finds it unpacked and says so without fetching.

    process, text = run_until_serving(
        project(tmp_path / "project"),
        python,
        {"MURU_OFFLINE": "1", "MURU_CACHE_DIR": str(cache)},
    )

    try:
        assert "fetched the runtime library" not in text
        assert f"muru: runtime library {wanted} from {unpacked}" in text
    finally:
        stop(process)


def test_offline_with_nothing_usable_names_the_ways_out(tmp_path: Path) -> None:
    python = bare_venv(tmp_path / "venv")
    completed = subprocess.run(
        [
            str(muructl()),
            "run",
            str(project(tmp_path / "project")),
            "--python",
            str(python),
            "--port",
            "auto",
        ],
        capture_output=True,
        text=True,
        timeout=60,
        env={**os.environ, "MURU_OFFLINE": "1", "MURU_CACHE_DIR": str(tmp_path / "cache")},
    )

    assert completed.returncode != 0
    assert "is not available" in completed.stderr
    assert "MURU_WHEEL_DIR" in completed.stderr
    assert f"install muru=={muructl_version()} into the environment" in completed.stderr


def test_runtime_install_unpacks_the_library_for_an_image(tmp_path: Path) -> None:
    wanted = muructl_version()
    wheels = build_wheel(tmp_path, wanted)
    python = bare_venv(tmp_path / "venv")
    target = tmp_path / "opt-muru"
    completed = subprocess.run(
        [
            str(muructl()),
            "runtime",
            "install",
            "--python",
            str(python),
            "--target",
            str(target),
            "--wheel-dir",
            str(wheels),
            "--offline",
        ],
        capture_output=True,
        text=True,
        timeout=60,
        check=True,
    )

    assert f"muru: unpacked muru-{wanted}-" in completed.stdout
    assert (target / "muru" / "__init__.py").is_file()

    imported = subprocess.run(
        [str(python), "-c", "import muru; print(muru.__version__)"],
        capture_output=True,
        text=True,
        check=True,
        env={**os.environ, "PYTHONPATH": str(target)},
    )
    assert imported.stdout.strip() == wanted
