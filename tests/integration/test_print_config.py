"""`muructl run --print-config` (design specification section 13.1): the
declarative equivalent of a development run, written without running it."""

from __future__ import annotations

import subprocess
import sys
import tomllib
from pathlib import Path

from support import server as support

PROJECTS = support.ROOT / "tests" / "fixtures" / "projects"


def print_config(*args: str) -> dict[str, object]:
    muructl = support.muructl_path()

    if muructl is None:
        import pytest

        pytest.skip("muructl is not built")

    completed = subprocess.run(
        [str(muructl), "run", *args, "--print-config", "--python", sys.executable],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout.startswith("# The declarative equivalent")
    document: dict[str, object] = tomllib.loads(completed.stdout)
    return document


def test_a_project_run_is_written_as_a_release() -> None:
    document = print_config(str(PROJECTS / "shop"), "--https", "--max-body-size", "25MB")
    release = document["app"]["shop"]["release"]["r1"]  # type: ignore[index]
    assert Path(release["source"]).resolve() == (PROJECTS / "shop").resolve()
    assert release["entrypoint"] == "shop.wsgi:application"
    assert release["processes"] == 2
    assert release["threads"] == 3
    assert release["hooks"] == "shop.hooks"
    assert release["preload"] == "auto"
    assert release["python"] == sys.executable
    assert "interface" not in release, "auto is the default, so it is not written"
    listeners = document["listener"]
    assert listeners == [{"address": "127.0.0.1:8080"}, {"address": "127.0.0.1:8443", "tls": True}]
    limits = document["limits"]
    assert limits["max_body_size"] == "25MB"  # type: ignore[index]
    assert limits["request_deadline"] == "300s"  # type: ignore[index]
    assert limits["max_header_bytes"] == "64KB"  # type: ignore[index]


def test_a_factory_entrypoint_keeps_its_factory_settings() -> None:
    document = print_config(str(PROJECTS / "factory"), "--interface", "wsgi")
    release = document["app"]["factory-site"]["release"]["r1"]  # type: ignore[index]
    assert release["factory"] is True
    assert release["factory_phase"] == "worker"
    assert release["interface"] == "wsgi"
    assert release["processes"] == 1


def test_recycling_and_host_timeouts_are_written() -> None:
    document = print_config(
        str(PROJECTS / "shop"),
        "--max-requests",
        "500",
        "--max-lifetime",
        "2h",
        "--timeout",
        "hard=45s",
        "--timeout",
        "soft=30s",
    )
    release = document["app"]["shop"]["release"]["r1"]  # type: ignore[index]
    assert release["max_requests"] == 500
    assert release["max_lifetime"] == "7200s"
    assert release["timeouts"] == {
        "soft_timeout": "30s",
        "hard_timeout": "45s",
        "hard_drain_grace": "10s",
        "shutdown_timeout": "5s",
        "loop_stall_timeout": "10s",
    }


def test_memory_settings_are_written() -> None:
    document = print_config(
        str(PROJECTS / "shop"), "--memory", "soft_limit=800MB", "--memory", "hard_limit=1200MB"
    )
    release = document["app"]["shop"]["release"]["r1"]  # type: ignore[index]
    assert release["memory"] == {
        "measure": "uss",
        "soft_limit": "800MB",
        "soft_for": "120s",
        "reclaim_first": True,
        "hard_limit": "1200MB",
        "check_interval": "5s",
    }
