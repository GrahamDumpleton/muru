"""Stopping the daemon with a signal (design specification section 13.3).
SIGTERM detaches by default, leaving the hosts and the edge serving for the
next daemon, and shuts everything down under `on_sigterm = 'shutdown'` in
the daemon file's `[shutdown]` table, the one sequence `muructl daemon stop
--shutdown` runs too: for the delay the edge keeps accepting while every
connection closes after its response, then the requests in flight finish,
then the workers stop and the edge last, all inside the budget, past which
what is left is killed."""

from __future__ import annotations

import http.client
import json
import signal
import subprocess
import threading
import time
from pathlib import Path

from support.daemon import SHOP, Daemon, start_daemon


def nothing_left_of(daemon: Daemon) -> bool:
    """Whether no process of the daemon's run directory remains."""

    listing = subprocess.run(
        ["ps", "-ax", "-o", "command="], capture_output=True, text=True, check=False
    ).stdout
    return not any(daemon.run_dir.name in line for line in listing.splitlines())


def request_in_the_background(daemon: Daemon, path: str) -> tuple[threading.Thread, list[str]]:
    """Request a path on a thread of its own, recording the status or the error."""

    outcome: list[str] = []

    def request() -> None:
        connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=60)

        try:
            connection.request("GET", path)
            outcome.append(f"status {connection.getresponse().status}")
        except (OSError, http.client.HTTPException) as error:
            outcome.append(type(error).__name__)
        finally:
            connection.close()

    thread = threading.Thread(target=request)
    thread.start()
    return thread, outcome


def test_sigterm_detaches_by_default(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        first.deploy("shop", SHOP, "--processes", "1")
        pid = json.loads(first.get("/info").body)["pid"]

        # The daemon goes on SIGTERM; the worker keeps answering.

        first.process.send_signal(signal.SIGTERM)
        assert first.process.wait(timeout=30) == 0
        assert "SIGTERM: detaching" in first.log()
        assert json.loads(first.get("/info").body)["pid"] == pid

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        assert json.loads(second.get("/info").body)["pid"] == pid
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        second.stop(shutdown=True)


def test_sigterm_shuts_down_under_the_table_and_a_request_in_flight_finishes(
    tmp_path: Path,
) -> None:
    daemon = start_daemon(
        tmp_path, config_extra="[shutdown]\non_sigterm = 'shutdown'\nbudget = '8s'\n"
    )

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")
        thread, outcome = request_in_the_background(daemon, "/slow?ms=2000")
        time.sleep(0.5)

        # Everything stops inside the budget, the request first.

        started = time.monotonic()
        daemon.process.send_signal(signal.SIGTERM)
        assert daemon.process.wait(timeout=30) == 0
        elapsed = time.monotonic() - started
        thread.join(timeout=10)
        assert outcome == ["status 200"]
        assert elapsed < 10, elapsed
        assert "SIGTERM: shutting down" in daemon.log()
        assert "stopped in" in daemon.log()
        assert nothing_left_of(daemon)
    finally:
        if daemon.process.poll() is None:
            daemon.stop()


def test_the_delay_closes_connections_while_the_edge_still_accepts(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path, config_extra="[shutdown]\non_sigterm = 'shutdown'\ndelay = '3s'\n"
    )
    connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=30)

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")

        # A keep-alive connection, served as usual before the signal.

        connection.request("GET", "/")
        response = connection.getresponse()
        response.read()
        assert response.status == 200
        assert response.getheader("Connection") != "close"

        # After it, the next response on the connection says to close,
        # while a new connection is still accepted and answered.

        daemon.process.send_signal(signal.SIGTERM)
        deadline = time.monotonic() + 2

        while True:
            connection.request("GET", "/")
            response = connection.getresponse()
            response.read()
            assert response.status == 200

            if response.getheader("Connection") == "close":
                break

            assert time.monotonic() < deadline, "no Connection: close within the delay"
            connection.close()
            connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=30)

        assert daemon.get("/").status == 200
        assert daemon.process.wait(timeout=30) == 0
        assert nothing_left_of(daemon)
    finally:
        connection.close()

        if daemon.process.poll() is None:
            daemon.stop()


def test_what_outruns_the_budget_is_killed(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path, config_extra="[shutdown]\non_sigterm = 'shutdown'\nbudget = '3s'\n"
    )

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")
        thread, outcome = request_in_the_background(daemon, "/slow?ms=30000")
        time.sleep(0.5)

        # The request cannot finish in time; the daemon still exits
        # cleanly at the budget, and nothing is left running.

        started = time.monotonic()
        daemon.process.send_signal(signal.SIGTERM)
        assert daemon.process.wait(timeout=30) == 0
        elapsed = time.monotonic() - started
        assert 2.5 < elapsed < 6, elapsed
        thread.join(timeout=10)
        assert outcome and outcome != ["status 200"], outcome
        assert "still in flight" in daemon.log()
        assert nothing_left_of(daemon)
    finally:
        if daemon.process.poll() is None:
            daemon.stop()


def test_daemon_stop_shutdown_runs_the_same_sequence(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra="[shutdown]\ndelay = '1s'\n")

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")
        thread, outcome = request_in_the_background(daemon, "/slow?ms=2000")
        time.sleep(0.5)
        started = time.monotonic()
        assert "stopped" in daemon.muructl("daemon", "stop", "--shutdown").stdout
        daemon.process.wait(timeout=30)
        elapsed = time.monotonic() - started
        thread.join(timeout=10)
        assert outcome == ["status 200"]
        assert 1 < elapsed < 10, elapsed
        assert "stopped in" in daemon.log()
        assert nothing_left_of(daemon)
    finally:
        if daemon.process.poll() is None:
            daemon.stop()


def test_reload_applies_the_shutdown_table(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra="[shutdown]\nbudget = '8s'\n")

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")
        config = tmp_path / "muru.toml"
        config.write_text(
            config.read_text().replace(
                "[shutdown]\nbudget = '8s'\n", "[shutdown]\non_sigterm = 'shutdown'\n"
            )
        )
        reloaded = daemon.muructl("daemon", "reload").stdout
        assert "[shutdown] (SIGTERM shuts down, delay 0s, budget 8s)" in reloaded, reloaded

        # And the same reload says it is unchanged.

        assert "[shutdown]" in daemon.muructl("daemon", "reload").stdout

        daemon.process.send_signal(signal.SIGTERM)
        assert daemon.process.wait(timeout=30) == 0
        assert "SIGTERM: shutting down" in daemon.log()
        assert nothing_left_of(daemon)
    finally:
        if daemon.process.poll() is None:
            daemon.stop()


def test_a_bad_shutdown_table_is_refused_at_start(tmp_path: Path) -> None:
    config = tmp_path / "muru.toml"
    config.write_text("[[listener]]\naddress = '127.0.0.1:0'\n\n[shutdown]\non_sigterm = 'drain'\n")
    muructl = Path(__file__).resolve().parents[2] / "target" / "debug" / "muructl"
    completed = subprocess.run(
        [str(muructl), "daemon", "run", "--config", str(config), "--run-dir", "/tmp/x"],
        capture_output=True,
        text=True,
        check=False,
        env={"MURU_STATE_DIR": str(tmp_path / "state"), "MURU_LOG_DIR": str(tmp_path / "logs")},
        timeout=60,
    )
    assert completed.returncode != 0
    assert "[shutdown]: on_sigterm = \"drain\" is neither 'shutdown' nor 'detach'" in (
        completed.stdout + completed.stderr
    )
