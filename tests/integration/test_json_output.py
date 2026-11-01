"""JSON output from muructl (design specification Appendix A).

Every command that reports something takes --json and prints the admin
API's answer with its field names, so scripts and coding agents read one
shape; a command that fails with --json prints its error as JSON too, with
the same exit status.
"""

from __future__ import annotations

import json
from typing import Any

from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def answer(daemon: Daemon, *arguments: str) -> Any:
    """Run a muructl command with --json and parse what it prints."""

    return json.loads(daemon.muructl(*arguments, "--json").stdout)


def test_read_commands_print_the_api_answers(daemon: Daemon) -> None:
    deployed = answer(daemon, "deploy", "shop", str(SHOP))
    assert (deployed["app"], deployed["id"], deployed["state"]) == ("shop", "r1", "ready")
    assert deployed["serving"] is True

    status = answer(daemon, "status")
    assert status["apps"] == 1
    assert f"127.0.0.1:{daemon.port}" in " ".join(status["listeners"])

    assert [app["name"] for app in answer(daemon, "app", "list")["apps"]] == ["shop"]
    releases = answer(daemon, "release", "list", "shop")["releases"]
    assert [release["id"] for release in releases] == ["r1"]
    assert answer(daemon, "release", "show", "shop@r1")["entrypoint"]

    for command, field in [
        ("host", "hosts"),
        ("route", "routes"),
        ("upstream", "upstreams"),
        ("overlay", "overlays"),
    ]:
        assert isinstance(answer(daemon, command, "list")[field], list), command

    assert answer(daemon, "rollout", "status")["rollouts"] == []
    assert answer(daemon, "scale", "status", "shop")["app"] == "shop"
    assert answer(daemon, "websockets")["open"] == 0
    assert "[app.shop.release.r1]" in answer(daemon, "config", "show")["document"]

    # Events and metrics: an event's type keeps its proto name, and each
    # sample carries the type its family declares.

    events = answer(daemon, "events")["events"]
    assert "release.ready" in [event["type"] for event in events]

    samples = answer(daemon, "metrics")["samples"]
    assert samples and all({"name", "type", "labels", "value"} <= set(sample) for sample in samples)
    assert any(sample["type"] in ("counter", "gauge", "histogram") for sample in samples)


def test_a_release_s_output_prints_as_json_lines(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.get("/")
    lines = daemon.muructl("logs", "shop", "--lines", "5", "--json").stdout.splitlines()

    assert lines

    for line in lines:
        record = json.loads(line)
        assert (record["app"], record["release"]) == ("shop", "r1")
        assert isinstance(record["line"], str)


def test_mutating_commands_print_what_they_changed(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)

    rolled_back = answer(daemon, "rollback", "shop")
    assert (rolled_back["id"], rolled_back["serving"]) == ("r1", True)

    maintenance = answer(daemon, "maintenance", "on", "shop")
    assert maintenance["on"] is True
    assert answer(daemon, "maintenance", "off", "shop")["on"] is False


def test_a_failure_prints_its_error_as_json(daemon: Daemon) -> None:
    failed = daemon.muructl("release", "show", "shop@r9", "--json", check=False)

    assert failed.returncode == 1
    error = json.loads(failed.stdout)["error"]
    assert isinstance(error, str) and error
