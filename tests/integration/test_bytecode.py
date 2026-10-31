"""Bytecode caches under the daemon (design specification section 8.2):
redirected to a directory of the release's own by default, left to Python
when the source is precompiled, or off."""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def compiled_files(directory: Path) -> list[Path]:
    return sorted(directory.rglob("*.pyc"))


def test_bytecode_is_redirected_to_the_release_directory(daemon: Daemon, tmp_path: Path) -> None:
    # A copy of the fixture without any bytecode, so the release is the
    # first to compile it.

    source = tmp_path / "shop"
    shutil.copytree(SHOP, source, ignore=shutil.ignore_patterns("__pycache__"))

    assert daemon.deploy("shop", source) == "muru: release shop@r1 is ready and serving"
    assert daemon.get("/").status == 200

    prefix = daemon.base / "state" / "pycache" / "shop" / "r1"
    assert compiled_files(prefix), "the host's imports were compiled under the release's directory"
    assert not compiled_files(source), "nothing landed in the source tree"

    shown = daemon.muructl("release", "show", "shop@r1").stdout
    assert f"bytecode     redirect, under {prefix}" in shown


def test_off_writes_none_and_source_uses_what_is_precompiled(
    daemon: Daemon, tmp_path: Path
) -> None:
    source = tmp_path / "shop"
    shutil.copytree(SHOP, source, ignore=shutil.ignore_patterns("__pycache__"))

    assert (
        daemon.deploy("shop", source, "--bytecode", "off")
        == "muru: release shop@r1 is ready and serving"
    )
    assert daemon.get("/").status == 200
    assert not (daemon.base / "state" / "pycache").exists()
    assert not compiled_files(source)
    assert "bytecode     off" in daemon.muructl("release", "show", "shop@r1").stdout

    # Precompiled by the interpreter the release runs on: used where it is,
    # and nothing is redirected.

    subprocess.run([sys.executable, "-m", "compileall", "-q", str(source)], check=True)
    assert compiled_files(source)

    assert (
        daemon.deploy("shop", source, "--release", "r2")
        == "muru: release shop@r2 is ready and serving"
    )
    assert (
        "bytecode     source (precompiled)" in daemon.muructl("release", "show", "shop@r2").stdout
    )
    assert not (daemon.base / "state" / "pycache" / "shop" / "r2").exists()
