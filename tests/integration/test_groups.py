"""Worker groups (design specification section 7.1).

A release splits its paths across named groups, each with its own process
and thread counts, all forked from one host and one preload. The most
specific group path wins and everything else goes to the default group;
each worker knows its group through the context and MURU_GROUP. Groups
come from the project's table in development mode and from the applied
file or the table under the daemon, and a restarted daemon takes every
group's workers back.
"""

from __future__ import annotations

import json
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon

GROUPED = support.ROOT / "tests" / "fixtures" / "projects" / "grouped"


def served_by(response: support.Response) -> dict[str, Any]:
    assert response.status == 200, response.body
    report: dict[str, Any] = json.loads(response.body)
    return report


@pytest.fixture(scope="module")
def development() -> Iterator[support.Server]:
    """The grouped project under muructl run."""

    instance = support.start(str(GROUPED), workers=None, threads=None, chdir=None)

    try:
        yield instance
    finally:
        instance.stop()


def test_paths_reach_their_groups_in_development(development: support.Server) -> None:
    root = served_by(development.get("/"))
    assert root["group"] == "default" and root["env_group"] == "default"

    reports = served_by(development.get("/reports/daily"))
    assert reports["group"] == "reports" and reports["env_group"] == "reports"
    assert served_by(development.get("/reportsx"))["group"] == "default", "segment boundary"

    status = served_by(development.get("/api/status"))
    assert status["group"] == "status"
    assert served_by(development.get("/api/status/extra"))["group"] == "status"
    assert served_by(development.get("/api"))["group"] == "default"

    # The reports group has two workers of its own, numbered within it.

    workers = {served_by(development.get("/reports/x"))["worker"] for _ in range(30)}
    assert workers == {0, 1}, workers
    assert served_by(development.get("/"))["worker"] == 0


def test_the_start_up_output_names_each_group(development: support.Server) -> None:
    log = development.log()
    assert "muru: serving grouped:application on" in log
    assert "with 1 worker of 2 threads" in log
    assert "muru: group reports serves /reports/* with 2 workers of 1 thread" in log
    assert "muru: group status serves /api/status with 1 worker of 8 threads" in log


def test_print_config_writes_the_groups() -> None:
    muructl = support.muructl_path()
    assert muructl is not None
    completed = subprocess.run(
        [str(muructl), "run", str(GROUPED), "--print-config", "--python", sys.executable],
        capture_output=True,
        text=True,
        check=True,
        timeout=30,
    )
    import tomllib

    release = tomllib.loads(completed.stdout)["app"]["grouped"]["release"]["r1"]
    assert release["groups"]["reports"] == {"paths": ["/reports/*"], "processes": 2, "threads": 1}
    assert release["groups"]["status"] == {"paths": ["/api/status"], "threads": 8}
    assert release["groups"]["slow"] == {
        "paths": ["/slow/*"],
        "timeouts": {"soft_timeout": "1s"},
        "memory": {"check_interval": "1s"},
    }


def test_a_group_has_timeouts_of_its_own(development: support.Server) -> None:
    # Section 7.1: the slow group's soft timeout interrupts its request;
    # the default group, with none, serves a request as long.

    slow = development.get("/slow/report?ms=1500", headers={"Accept": "application/json"})
    assert slow.status == 504, slow.body
    assert json.loads(slow.body)["reason"] == "soft_timeout"
    assert served_by(development.get("/?ms=1500"))["group"] == "default"
    assert served_by(development.get("/slow/report?ms=100"))["group"] == "slow"


@pytest.fixture
def daemon(tmp_path: Path) -> Iterator[Daemon]:
    instance = start_daemon(tmp_path)

    try:
        yield instance
    finally:
        instance.stop()


def test_groups_under_the_daemon_from_the_table_and_the_file(
    daemon: Daemon, tmp_path: Path
) -> None:
    # Deployed from its directory, the project's own groups apply.

    assert daemon.deploy("grouped", GROUPED) == "muru: release grouped@r1 is ready and serving"
    assert served_by(daemon.request("/reports/x"))["group"] == "reports"
    assert served_by(daemon.request("/"))["group"] == "default"
    assert "5 workers ready" in daemon.muructl("events").stdout
    assert daemon.request("/slow/x?ms=1500").status == 504
    shown = daemon.muructl("release", "show", "grouped@r1").stdout
    assert "workers 1 process of 2 threads group reports" in " ".join(shown.split())
    assert "group reports /reports/*: 2 processes of 1 thread" in " ".join(shown.split())

    # A declared release's groups replace the table's, and resolve their
    # sizes from the release where silent; the export writes them whole.

    config = tmp_path / "shop.toml"
    config.write_text(
        f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
threads = 5

[app.shop.release.r1.groups.hot]
paths = ["/hot/*", "/also-hot"]
processes = 1
timeouts = {{ hard_timeout = "300s" }}
memory = {{ hard_limit = "1GB" }}
"""
    )
    applied = daemon.muructl("apply", "-f", str(config))
    assert "ready and serving" in applied.stdout, applied.stdout
    hot = served_by(daemon.request("/hot/thing"))
    assert hot["group"] == "hot" and hot["app"] == "shop"
    assert served_by(daemon.request("/also-hot"))["group"] == "hot"
    assert served_by(daemon.request("/"))["group"] == "default"

    import tomllib

    exported = tomllib.loads(daemon.muructl("config", "export", "--app", "shop").stdout)
    groups = exported["app"]["shop"]["release"]["r1"]["groups"]
    assert groups == {
        "hot": {
            "paths": ["/hot/*", "/also-hot"],
            "processes": 1,
            "threads": 5,
            "sync_threads": 8,
            "concurrency": 1000,
            "timeouts": {"hard_timeout": "300s"},
            "memory": {
                "measure": "uss",
                "soft_for": "120s",
                "reclaim_first": True,
                "hard_limit": "1GB",
                "check_interval": "5s",
            },
        }
    }
    assert "no changes" in daemon.muructl("apply", "-f", str(config)).stdout

    # Changed groups are a different release.

    config.write_text(config.read_text().replace("processes = 1", "processes = 2"))
    refused = daemon.muructl("apply", "-f", str(config), check=False)
    assert refused.returncode == 1
    assert (
        "groups hot 2x5 on /hot/* /also-hot timeouts hard_timeout 300s memory uss soft off "
        "for 120s (reclaim first true), hard 1GB, every 5s, recorded hot 1x5"
    ) in refused.stdout, refused.stdout


def test_invalid_groups_are_refused(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "shop.toml"

    for table, expected in [
        ("[app.shop.release.r1.groups.default]\npaths = ['/x']\n", "is not a valid worker group"),
        ("[app.shop.release.r1.groups.hot]\n", "has no paths"),
        ("[app.shop.release.r1.groups.hot]\npaths = ['x']\n", "does not start with /"),
        ("[app.shop.release.r1.groups.hot]\npaths = ['/x']\nprocesses = 0\n", "has no processes"),
        (
            "[app.shop.release.r1.groups.hot]\npaths = ['/x']\n"
            "timeouts = { shutdown_timeout = '1s' }\n",
            "worker group hot: timeouts.shutdown_timeout applies to the whole release",
        ),
        (
            "[app.shop.release.r1.groups.hot]\npaths = ['/x']\n"
            "timeouts = { soft_timeout = '10s', hard_timeout = '12s' }\n",
            "worker group hot: hard_timeout must exceed soft_timeout by at least 5s",
        ),
    ]:
        config.write_text(
            f"[app.shop.release.r1]\nsource = '{SHOP}'\npython = '{sys.executable}'\n\n{table}"
        )
        refused = daemon.muructl("apply", "-f", str(config), check=False)
        assert refused.returncode == 1, refused.stdout
        assert expected in refused.stdout, refused.stdout

    assert daemon.releases() == []


def test_a_restarted_daemon_readopts_every_group(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)

    try:
        first.deploy("grouped", GROUPED)
        assert served_by(first.request("/reports/x"))["group"] == "reports"
        first.stop(shutdown=False)

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        try:
            assert "re-adopted the host" in second.log()
            assert served_by(second.request("/reports/x"))["group"] == "reports"
            assert served_by(second.request("/"))["group"] == "default"
        finally:
            second.stop()
    finally:
        if first.process.poll() is None:
            first.stop()
