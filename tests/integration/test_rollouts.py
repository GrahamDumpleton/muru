"""Rollouts (design specification sections 9.2 and 9.3).

A release deployed with --hold runs without traffic and can be previewed.
A canary rollout gives it a growing share of the application's requests
step by step; each step is judged by gates against the release that serves,
a failing gate rolls back and the last step passing promotes. Rollouts can
be paused, resumed, approved step by step, promoted and aborted by hand,
and a blue/green promotion moves all traffic at once.
"""

from __future__ import annotations

import json
import sys
import time
import tomllib
from pathlib import Path

from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def prepare(daemon: Daemon, tmp_path: Path) -> None:
    """r1 serving, r2 held, and a host with one route to the shop."""

    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    held = daemon.muructl(
        "deploy", "shop", str(SHOP), "--python", sys.executable, "--hold"
    ).stdout.strip()
    assert held == "muru: release shop@r2 is ready and held: not serving"
    config = tmp_path / "hosts.toml"
    config.write_text(
        """[host.shop]
names = ["shop.test"]
default = true

[[route]]
host = "shop"
app = "shop"
"""
    )
    assert daemon.muructl("apply", "-f", str(config)).returncode == 0


def served_by(daemon: Daemon, path: str = "/", **headers: str) -> str:
    response = daemon.request(path, host="shop.test", headers=headers)
    assert response.status == 200, response.body
    release: str = json.loads(response.body)["release"]
    return release


def releases_seen(daemon: Daemon, count: int, prefix: str) -> set[str]:
    return {served_by(daemon, Cookie=f"muru_id={prefix}-{n}") for n in range(count)}


def states(daemon: Daemon) -> dict[str, tuple[str, str]]:
    return {row["release"]: (row["state"], row["serving"]) for row in daemon.releases("shop")}


def wait_for_states(
    daemon: Daemon, expected: dict[str, tuple[str, str]], timeout: float = 10
) -> None:
    """The releases reach these states within the timeout; drains take a moment."""

    deadline = time.monotonic() + timeout

    while states(daemon) != expected and time.monotonic() < deadline:
        time.sleep(0.2)

    assert states(daemon) == expected


def rollout_state(daemon: Daemon) -> str:
    rows = daemon.muructl("rollout", "status", "shop").stdout.splitlines()[1:]
    return rows[-1].split()[4]


def wait_for_rollout(daemon: Daemon, state: str, timeout: float = 30) -> None:
    deadline = time.monotonic() + timeout

    while rollout_state(daemon) != state and time.monotonic() < deadline:
        time.sleep(0.5)

    assert rollout_state(daemon) == state, daemon.muructl("rollout", "status", "shop").stdout


def test_a_held_release_takes_no_traffic_but_previews(daemon: Daemon, tmp_path: Path) -> None:
    prepare(daemon, tmp_path)
    assert states(daemon) == {"shop@r1": ("ready", "yes"), "shop@r2": ("ready", "")}
    assert releases_seen(daemon, 20, "held") == {"r1"}
    assert served_by(daemon, **{"Muru-Preview": "r2"}) == "r2"

    # A held release is not drained by a configuration change.

    assert daemon.muructl("apply", "-f", str(tmp_path / "hosts.toml")).returncode == 0
    assert states(daemon)["shop@r2"] == ("ready", "")

    # The blue/green promotion moves everything at once.

    promoted = daemon.muructl("rollout", "bluegreen", "shop@r2").stdout.strip()
    assert promoted == "muru: release shop@r2 is ready and serving"
    wait_for_states(daemon, {"shop@r1": ("retained", ""), "shop@r2": ("ready", "yes")})
    assert releases_seen(daemon, 20, "after") == {"r2"}


def test_a_canary_promotes_once_its_gates_pass(daemon: Daemon, tmp_path: Path) -> None:
    prepare(daemon, tmp_path)
    started = daemon.muructl(
        "rollout", "canary", "shop@r2", "--steps", "50", "--hold", "2s", "--min-requests", "20"
    ).stdout
    assert "rollout 1 of shop@r2 started against r1: step 1 of 1: 50% for 2s" in started, started
    assert rollout_state(daemon) == "running"
    assert "50%" in daemon.muructl("rollout", "status").stdout

    # Half the clients land on the candidate; after the hold, with enough
    # requests and no errors, it is promoted.

    assert releases_seen(daemon, 100, "canary") == {"r1", "r2"}
    waited = daemon.muructl("rollout", "status", "shop", "--wait").stdout
    assert "promoted: every step passed its gates" in waited, waited
    wait_for_states(daemon, {"shop@r1": ("retained", ""), "shop@r2": ("ready", "yes")})
    assert releases_seen(daemon, 20, "done") == {"r2"}
    events = daemon.events()
    assert "rollout.started" in events and "rollout.completed" in events
    assert "rollout.rolled_back" not in events

    # A second canary needs a different release.

    refused = daemon.muructl("rollout", "canary", "shop@r2", check=False)
    assert refused.returncode != 0 and "already serves" in refused.stderr, refused.stderr


def test_a_failing_gate_rolls_the_canary_back(daemon: Daemon, tmp_path: Path) -> None:
    prepare(daemon, tmp_path)
    daemon.muructl(
        "rollout",
        "canary",
        "shop@r2",
        "--steps",
        "50",
        "--hold",
        "60s",
        "--min-requests",
        "20",
        "--max-error-rate",
        "10",
    )

    # The candidate fails a third of what it is asked, through previews
    # that pin the failing requests to it; the baseline is fine.

    for n in range(30):
        failed = daemon.request("/error", host="shop.test", headers={"Muru-Preview": "r2"})
        assert failed.status == 500, failed.status
        assert served_by(daemon, **{"Muru-Preview": "r2"}) == "r2"
        assert served_by(daemon, Cookie=f"muru_id=base-{n}") in {"r1", "r2"}

    wait_for_rollout(daemon, "rolled_back")
    status = daemon.muructl("rollout", "status", "shop").stdout
    assert "gate failed at 50%: 5xx rate" in status, status
    wait_for_states(daemon, {"shop@r1": ("ready", "yes"), "shop@r2": ("retained", "")})
    assert releases_seen(daemon, 20, "after") == {"r1"}
    events = daemon.events()
    assert "rollout.gate_failed" in events and "rollout.rolled_back" in events


def test_a_worker_crash_rolls_the_canary_back_at_once(daemon: Daemon, tmp_path: Path) -> None:
    prepare(daemon, tmp_path)
    daemon.muructl("rollout", "canary", "shop@r2", "--steps", "50", "--hold", "60s")

    # One crash, long before the step has the requests the other gates
    # need, decides.

    crashed = daemon.request("/crash", host="shop.test", headers={"Muru-Preview": "r2"})
    assert crashed.status >= 500, crashed.status
    wait_for_rollout(daemon, "rolled_back")
    status = daemon.muructl("rollout", "status", "shop").stdout
    assert "gate failed at 50%: 1 worker crashes and timeout kills" in status, status


def test_a_probe_failing_on_the_candidate_alone_rolls_back(daemon: Daemon, tmp_path: Path) -> None:
    prepare(daemon, tmp_path)
    probe = f"http://127.0.0.1:{daemon.port}/fails-on/r2"
    daemon.muructl(
        "rollout", "canary", "shop@r2", "--steps", "50", "--hold", "60s", "--probe", probe
    )

    # No request at all, and the probe decides.

    wait_for_rollout(daemon, "rolled_back")
    status = daemon.muructl("rollout", "status", "shop").stdout
    assert "gate failed at 50%: 1 failed probes (baseline 0)" in status, status
    assert f"{probe} answered 500" in status, status

    refused = daemon.muructl("rollout", "canary", "shop@r2", "--probe", "/health", check=False)
    assert "not an http or https URL with a host" in refused.stderr


def test_a_rollout_needs_no_route_for_the_default_application(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.muructl("deploy", "shop", str(SHOP), "--python", sys.executable, "--hold")
    daemon.muructl(
        "rollout", "canary", "shop@r2", "--steps", "50", "--hold", "60s", "--min-requests", "1000"
    )

    # With no host declared, the application's requests are shared
    # between the releases as a route's would be, and a preview reaches
    # the candidate.

    def release(**headers: str) -> str:
        response = daemon.request("/", headers=headers)
        assert response.status == 200, response.body
        served: str = json.loads(response.body)["release"]
        return served

    seen = {release(Cookie=f"muru_id=default-{n}") for n in range(40)}
    assert seen == {"r1", "r2"}, seen
    assert release(**{"Muru-Preview": "r2", "Cookie": "muru_id=default-0"}) == "r2"

    daemon.muructl("rollout", "abort", "shop")
    assert {release(Cookie=f"muru_id=default-{n}") for n in range(20)} == {"r1"}


def test_an_export_can_resolve_a_rollout_where_it_stands(daemon: Daemon, tmp_path: Path) -> None:
    prepare(daemon, tmp_path)
    daemon.muructl(
        "rollout", "canary", "shop@r2", "--steps", "30", "--hold", "60s", "--min-requests", "1000"
    )

    # The plain export has the serving release alone and no split.

    plain = tomllib.loads(daemon.muructl("config", "export").stdout)
    assert list(plain["app"]["shop"]["release"]) == ["r1"]
    assert "traffic" not in plain["route"][0]

    resolved = tomllib.loads(daemon.muructl("config", "export", "--resolve-rollouts").stdout)
    assert sorted(resolved["app"]["shop"]["release"]) == ["r1", "r2"]
    assert resolved["route"][0]["traffic"]["split"] == {"r1": 70, "r2": 30}

    # Applied back, the split holds the same share once the rollout ends.

    exported = tmp_path / "resolved.toml"
    exported.write_text(daemon.muructl("config", "export", "--resolve-rollouts").stdout)
    daemon.muructl("rollout", "abort", "shop")
    daemon.muructl("apply", "-f", str(exported))
    seen = releases_seen(daemon, 60, "resolved")
    assert seen == {"r1", "r2"}, seen


def test_rollouts_pause_resume_approve_promote_and_abort(daemon: Daemon, tmp_path: Path) -> None:
    prepare(daemon, tmp_path)
    daemon.muructl(
        "rollout",
        "canary",
        "shop@r2",
        "--steps",
        "10,50",
        "--hold",
        "1s",
        "--min-requests",
        "1",
        "--approve-each-step",
    )
    assert releases_seen(daemon, 30, "first") >= {"r1"}
    wait_for_rollout(daemon, "awaiting_approval")
    assert "awaits approval" in daemon.muructl("rollout", "status", "shop").stdout

    # Traffic stays at the first step's share until the next is approved.

    refused = daemon.muructl("rollout", "pause", "shop", check=False)
    assert refused.returncode != 0 and "awaiting_approval" in refused.stderr
    resumed = daemon.muructl("rollout", "resume", "shop").stdout
    assert "running: step 2 of 2: 50% for 1s" in resumed, resumed
    paused = daemon.muructl("rollout", "pause", "shop").stdout
    assert "paused: paused at 50%" in paused, paused
    time.sleep(2)
    assert rollout_state(daemon) == "paused", "a paused rollout does not move"
    assert "running" in daemon.muructl("rollout", "resume", "shop").stdout
    promoted = daemon.muructl("rollout", "promote", "shop").stdout
    assert "promoted: promoted by hand at 50%" in promoted, promoted
    wait_for_states(daemon, {"shop@r1": ("retained", ""), "shop@r2": ("ready", "yes")})

    # Aborting takes the candidate out and keeps it warm, which stops the
    # older retained release: one retained release per application.

    daemon.muructl("deploy", "shop", str(SHOP), "--python", sys.executable, "--hold")
    daemon.muructl(
        "rollout", "canary", "shop@r3", "--steps", "50", "--hold", "60s", "--min-requests", "1000"
    )
    assert releases_seen(daemon, 30, "third") == {"r2", "r3"}
    aborted = daemon.muructl("rollout", "abort", "shop").stdout
    assert "rolled_back: aborted by hand at 50%" in aborted, aborted
    wait_for_states(
        daemon,
        {"shop@r1": ("stopped", ""), "shop@r2": ("ready", "yes"), "shop@r3": ("retained", "")},
    )
    assert releases_seen(daemon, 20, "after") == {"r2"}

    listing = daemon.muructl("rollout", "status", "shop").stdout
    assert listing.count("shop ") == 2 and "promoted" in listing and "rolled_back" in listing

    for arguments, complaint in [
        (["shop@r3", "--steps", "50,10"], "the shares must increase"),
        (["shop@r3", "--steps", "0"], "1 to 100"),
        (["shop@r9"], "does not exist"),
    ]:
        refused = daemon.muructl("rollout", "canary", *arguments, check=False)
        assert refused.returncode != 0 and complaint in refused.stderr, refused.stderr
