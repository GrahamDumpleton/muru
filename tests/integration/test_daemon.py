"""The daemon (design specification sections 9.1, 9.5, 10 and 13.3).

A daemon runs with its own state, runtime and log directories; releases are
deployed from source directories with the blue/green strategy, traffic
moves when the new release is ready and the previous one drains, rollback
restores the previous release, and every state change is an event.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from support import server as support
from support.daemon import FACTORY, SHOP, Daemon, start_daemon
from support.daemon import daemon as daemon_fixture
from support.load import Load

daemon = daemon_fixture


def test_status_before_any_release(daemon: Daemon) -> None:
    status = daemon.muructl("status").stdout

    assert f"http://127.0.0.1:{daemon.port}" in status
    assert "apps     0 (0 releases)" in status
    assert daemon.get("/").status == 503


def test_deploy_serves_and_records_the_release(daemon: Daemon) -> None:
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"

    response = daemon.get("/")
    assert response.status == 200
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]
    assert "shop" in daemon.muructl("app", "list").stdout
    assert daemon.events()[-3:] == ["release.created", "release.ready", "traffic.switched"]


def test_second_deploy_moves_traffic_without_failed_requests(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    pid_before = json.loads(daemon.get("/info").body)["pid"]

    # Requests keep flowing while the deploy happens, on kept-alive
    # connections as real clients use (and so the test does not run out
    # of ephemeral ports).

    with Load(daemon.port) as load:
        assert (
            daemon.deploy("shop", SHOP, "--processes", "2")
            == "muru: release shop@r2 is ready and serving"
        )
        time.sleep(0.5)

    load.check("the deploy")
    assert json.loads(daemon.get("/info").body)["pid"] != pid_before
    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "ready", "serving": "yes"},
        {"release": "shop@r1", "state": "retained", "serving": ""},
    ]


def test_rollback_restores_the_previous_release(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP, "--release", "second")

    assert (
        daemon.muructl("rollback", "shop").stdout.strip() == "muru: rolled back to shop@r1 (ready)"
    )
    assert daemon.get("/").status == 200
    assert daemon.releases("shop") == [
        {"release": "shop@second", "state": "retained", "serving": ""},
        {"release": "shop@r1", "state": "ready", "serving": "yes"},
    ]
    assert "APP                  CURRENT      PREVIOUS" in daemon.muructl("app", "list").stdout
    assert "shop                 r1           second" in daemon.muructl("app", "list").stdout


def test_rollback_without_a_previous_release_is_refused(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    completed = daemon.muructl("rollback", "shop", check=False)

    assert completed.returncode != 0
    assert "no previous release" in completed.stderr


def test_drain_takes_the_release_out_of_traffic(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)

    assert daemon.muructl("drain", "shop@r1").stdout.strip() == "muru: release shop@r1 is stopped"
    assert daemon.get("/").status == 503
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "stopped", "serving": ""}]


def test_failed_deploy_keeps_the_previous_release_serving(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    completed = daemon.muructl(
        "deploy",
        "shop",
        str(SHOP),
        "--python",
        sys.executable,
        "--entrypoint",
        "shop.wsgi:missing",
        check=False,
    )

    assert completed.returncode != 0
    assert "host exited" in completed.stderr or "failed" in completed.stderr
    assert daemon.get("/").status == 200
    assert daemon.releases("shop")[0] == {"release": "shop@r2", "state": "failed", "serving": ""}
    assert "release.failed" in daemon.events()


def test_settings_come_from_the_table_and_flags(daemon: Daemon) -> None:
    # The shop project's pyproject.toml sets processes and threads; the
    # factory project's muru.toml names a factory entrypoint.

    daemon.deploy("shop", SHOP)
    assert "2x3" in daemon.muructl("release", "list", "shop").stdout

    daemon.deploy("factory", FACTORY, "--threads", "2")
    listing = daemon.muructl("release", "list", "factory").stdout
    assert "1x2" in listing
    assert daemon.releases("factory")[0]["state"] == "ready"


def test_release_wait_and_no_wait(daemon: Daemon) -> None:
    line = daemon.deploy("shop", SHOP, "--no-wait")
    assert line.startswith("muru: release shop@r1 recorded")

    waited = daemon.muructl("release", "wait", "shop@r1", "--timeout", "60").stdout.strip()
    assert waited == "muru: release shop@r1 is ready"
    assert daemon.get("/").status == 200


def test_a_dead_host_is_started_again(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    pid = json.loads(daemon.get("/info").body)["pid"]

    # The host writes to its own log; its pid is in the daemon's record.

    record = (daemon.run_dir / "apps" / "shop" / "r1" / "host.toml").read_text()
    host_pid = int(
        next(line.split("=")[1] for line in record.splitlines() if line.startswith("pid"))
    )
    os.kill(host_pid, signal.SIGKILL)

    deadline = time.monotonic() + 30

    while time.monotonic() < deadline:
        if "release.restarting" in daemon.events() and daemon.get("/").status == 200:
            break
        time.sleep(0.2)

    assert daemon.get("/").status == 200
    assert json.loads(daemon.get("/info").body)["pid"] != pid
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]


def test_current_release_is_started_again_after_a_shutdown(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)

    try:
        first.deploy("shop", SHOP)
        assert first.get("/").status == 200
    finally:
        first.stop(shutdown=True)

    second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

    try:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and second.get("/").status != 200:
            time.sleep(0.2)
        assert second.get("/").status == 200
        assert second.releases("shop") == [
            {"release": "shop@r1", "state": "ready", "serving": "yes"}
        ]
    finally:
        second.stop()


def edge_pid_of(daemon: Daemon) -> int:
    """The edge process pid from the status output."""

    status = daemon.muructl("status").stdout
    return int(next(line.split()[2] for line in status.splitlines() if line.startswith("edge")))


def test_detached_daemon_leaves_traffic_flowing_and_is_readopted(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        # One worker, so that the pid each answer carries is the one worker's.

        first.deploy("shop", SHOP, "--processes", "1")
        pid = json.loads(first.get("/info").body)["pid"]
        edge_pid = edge_pid_of(first)

        # The daemon goes; the edge and the host do not.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        assert first.get("/").status == 200
        assert json.loads(first.get("/info").body)["pid"] == pid

        # The next daemon takes them over, with no gap and no new processes.

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        assert edge_pid_of(second) == edge_pid
        assert json.loads(second.get("/info").body)["pid"] == pid
        assert second.releases("shop") == [
            {"release": "shop@r1", "state": "ready", "serving": "yes"}
        ]
        assert "release.adopted" in second.events()

        # And deploys keep working through the adopted edge.

        assert second.deploy("shop", SHOP) == "muru: release shop@r2 is ready and serving"
        assert json.loads(second.get("/info").body)["pid"] != pid
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        second.stop(shutdown=True)

    listing = subprocess.run(
        ["ps", "-ax", "-o", "command="], capture_output=True, text=True, check=False
    ).stdout
    assert not any(first.run_dir.name in line for line in listing.splitlines())


def test_a_relative_interpreter_path_is_resolved_where_the_command_runs(
    daemon: Daemon,
) -> None:
    # The daemon starts the host elsewhere, so a path relative to the
    # shell is made absolute by the client; a bare name stays a command.

    relative = os.path.relpath(sys.executable)
    assert not os.path.isabs(relative) and os.sep in relative
    deployed = daemon.muructl("deploy", "shop", str(SHOP), "--python", relative)
    assert deployed.stdout.strip() == "muru: release shop@r1 is ready and serving"
    inspected = daemon.muructl("debug", "inspect", "shop").stdout
    assert f"at {os.path.abspath(relative)}," in inspected, inspected


def test_daemon_stop_gives_up_on_a_silent_socket(tmp_path: Path) -> None:
    """A socket that accepts connections and never answers cannot hold
    daemon stop past its timeout: every exchange is bounded."""

    muructl = support.muructl_path()
    if muructl is None:
        support.unavailable("muructl is not built")
    run_dir = Path(tempfile.mkdtemp(prefix="muru-run-", dir="/tmp"))
    path = run_dir / "admin.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(path))
    listener.listen(1)

    try:
        started = time.monotonic()
        stopped = subprocess.run(
            [str(muructl), "daemon", "stop", "--timeout", "2s", "--socket", str(path)],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        elapsed = time.monotonic() - started
    finally:
        listener.close()

    assert stopped.returncode != 0
    assert "did not answer within 2 seconds" in stopped.stderr, stopped.stderr
    assert elapsed < 20, elapsed


def test_background_daemon_start_status_stop(tmp_path: Path) -> None:
    muructl = support.muructl_path()
    if muructl is None or not support.extension_available():
        support.unavailable("muructl or the extension is not built")
    run_dir = Path(tempfile.mkdtemp(prefix="muru-run-", dir="/tmp"))
    port = support.free_port()
    location = [
        "--state-dir",
        str(tmp_path / "state"),
        "--run-dir",
        str(run_dir),
        "--log-dir",
        str(tmp_path / "logs"),
    ]

    started = subprocess.run(
        [str(muructl), "daemon", "start", "--listen", f"127.0.0.1:{port}", *location],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )

    try:
        assert started.returncode == 0, started.stdout + started.stderr
        assert "daemon started" in started.stdout
        again = subprocess.run(
            [str(muructl), "up", *location], capture_output=True, text=True, check=False, timeout=60
        )
        assert "already running" in again.stdout
        status = subprocess.run(
            [str(muructl), "daemon", "status", *location],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        assert f"http://127.0.0.1:{port}" in status.stdout
    finally:
        stopped = subprocess.run(
            [str(muructl), "daemon", "stop", "--shutdown", *location],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )

    assert "daemon stopped" in stopped.stdout, stopped.stdout + stopped.stderr
    assert "releases drained and stopped" in stopped.stdout
    assert (tmp_path / "logs" / "daemon.log").is_file()


def test_status_names_the_edge_process(daemon: Daemon) -> None:
    status = daemon.muructl("status").stdout
    edge_pid = int(next(line.split()[2] for line in status.splitlines() if line.startswith("edge")))

    assert edge_pid != daemon.process.pid
    assert os.path.exists(f"/proc/{edge_pid}") or os.kill(edge_pid, 0) is None


def test_a_dead_edge_is_started_again_without_refusing_connections(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    status = daemon.muructl("status").stdout
    edge_pid = int(next(line.split()[2] for line in status.splitlines() if line.startswith("edge")))

    # The daemon holds the listener too, so a connection made while no
    # edge runs waits in the backlog and is served by the new edge.

    os.kill(edge_pid, signal.SIGKILL)
    time.sleep(0.2)
    assert daemon.get("/").status == 200

    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and "edge.restarted" not in daemon.events():
        time.sleep(0.1)
    assert "edge.restarted" in daemon.events()
    new_status = daemon.muructl("status").stdout
    new_pid = int(
        next(line.split()[2] for line in new_status.splitlines() if line.startswith("edge"))
    )
    assert new_pid != edge_pid


def test_superseded_release_is_retained_and_rollback_is_instant(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    first_pid = json.loads(daemon.get("/info").body)["pid"]
    daemon.deploy("shop", SHOP, "--processes", "1")

    # The first release is warm, with its workers untouched.

    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "ready", "serving": "yes"},
        {"release": "shop@r1", "state": "retained", "serving": ""},
    ]
    assert "warm until" in daemon.muructl("release", "list", "shop").stdout

    assert (
        daemon.muructl("rollback", "shop").stdout.strip() == "muru: rolled back to shop@r1 (ready)"
    )
    assert json.loads(daemon.get("/info").body)["pid"] == first_pid
    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "retained", "serving": ""},
        {"release": "shop@r1", "state": "ready", "serving": "yes"},
    ]


def test_retention_can_be_switched_off_or_shortened(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP, "--retain", "0")
    assert daemon.releases("shop")[1] == {"release": "shop@r1", "state": "stopped", "serving": ""}

    daemon.deploy("shop", SHOP, "--retain", "1s")
    assert daemon.releases("shop")[1] == {"release": "shop@r2", "state": "retained", "serving": ""}
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and daemon.releases("shop")[1]["state"] != "stopped":
        time.sleep(0.2)
    assert daemon.releases("shop")[1] == {"release": "shop@r2", "state": "stopped", "serving": ""}
    assert "the retention window passed" in daemon.muructl("release", "list", "shop").stdout


def test_only_one_release_is_retained_per_application(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)

    assert [row["state"] for row in daemon.releases("shop")] == ["ready", "retained", "stopped"]
    assert "superseded again" in daemon.muructl("release", "list", "shop").stdout
