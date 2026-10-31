"""The runtime library provided by muructl (design specification section
8.3): the environment's own `muru` when it matches muructl's version, else
an unpacked wheel on the host's PYTHONPATH, fetched from a wheel directory
when the network is off limits, and `muructl runtime install` for images."""

from __future__ import annotations

import os
import subprocess
import sys
from importlib.metadata import version
from pathlib import Path

import pytest
from support import provisioning as prov


def run_until_serving(
    directory: Path, python: Path, env: dict[str, str]
) -> tuple[subprocess.Popen[bytes], str]:
    """Start muructl run on the project with that interpreter and return
    the process once it serves, with its output so far."""

    process, log = prov.run_until_serving(directory, env, python=python)
    return process, log.read_text(errors="replace")


def test_a_matching_environment_copy_is_used_and_nothing_fetched(tmp_path: Path) -> None:
    if version("muru") != prov.muructl_version():
        pytest.skip("the development virtualenv's muru does not match muructl (run just dev)")

    process, text = run_until_serving(
        prov.project(tmp_path / "project"),
        Path(sys.executable),
        {"MURU_OFFLINE": "1", "MURU_CACHE_DIR": str(tmp_path / "cache")},
    )

    try:
        assert f"muru: runtime library {version('muru')} from the environment" in text
        assert "fetched the runtime library" not in text
    finally:
        prov.stop(process)

    assert not (tmp_path / "cache" / "runtime").exists()


def test_an_environment_without_muru_gets_the_library_from_the_wheel_directory(
    tmp_path: Path,
) -> None:
    wanted = prov.muructl_version()
    wheels = prov.build_wheel(tmp_path, wanted)
    python = prov.bare_venv(tmp_path / "venv")
    cache = tmp_path / "cache"
    process, text = run_until_serving(
        prov.project(tmp_path / "project"),
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
        prov.stop(process)

    # The second run finds it unpacked and says so without fetching.

    process, text = run_until_serving(
        prov.project(tmp_path / "project"),
        python,
        {"MURU_OFFLINE": "1", "MURU_CACHE_DIR": str(cache)},
    )

    try:
        assert "fetched the runtime library" not in text
        assert f"muru: runtime library {wanted} from {unpacked}" in text
    finally:
        prov.stop(process)


def test_offline_with_nothing_usable_names_the_ways_out(tmp_path: Path) -> None:
    python = prov.bare_venv(tmp_path / "venv")
    completed = subprocess.run(
        [
            str(prov.muructl()),
            "run",
            str(prov.project(tmp_path / "project")),
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
    assert f"install muru=={prov.muructl_version()} into the environment" in completed.stderr


def test_runtime_install_unpacks_the_library_for_an_image(tmp_path: Path) -> None:
    wanted = prov.muructl_version()
    wheels = prov.build_wheel(tmp_path, wanted)
    python = prov.bare_venv(tmp_path / "venv")
    target = tmp_path / "opt-muru"
    completed = subprocess.run(
        [
            str(prov.muructl()),
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
