"""Worker inspection (design specification section 11.7): `muructl debug`
asks a release's workers for their status, Python stacks with the request
each thread serves, asyncio tasks and what they have loaded, through the
host's control connection, and `muructl logging level` changes a logger's
level for a period. A worker that cannot answer is reported as such."""

from __future__ import annotations

import http.client
import json
import os
import signal
import threading
import time
from collections.abc import Callable

from support import server as support
from support.daemon import Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def wait_for(predicate: Callable[[], bool], timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def deploy_plain(daemon: Daemon, processes: int = 2) -> None:
    assert daemon.deploy(
        "plain",
        support.FIXTURES,
        "--entrypoint",
        "plain_wsgi:application",
        "--processes",
        str(processes),
        "--threads",
        "2",
    ) == ("muru: release plain@r1 is ready and serving")


def test_status_lists_every_worker_with_its_figures(daemon: Daemon) -> None:
    deploy_plain(daemon)
    pids = {json.loads(daemon.get("/info").body)["pid"] for _ in range(20)}

    def status_rows() -> list[list[str]]:
        lines = daemon.muructl("debug", "status", "plain").stdout.splitlines()
        assert lines[0].split()[:4] == ["WORKER", "PID", "GROUP", "UP"]
        return [line.split() for line in lines[1:]]

    # The figures come from the workers' reports, once a second.

    wait_for(lambda: sum(int(row[4]) for row in status_rows()) >= 20)
    rows = status_rows()
    assert [row[0] for row in rows] == ["0", "1"]
    assert {int(row[1]) for row in rows} == pids
    assert all(row[2] == "default" and row[-1] == "ready" for row in rows)

    answer = json.loads(daemon.muructl("debug", "status", "plain@r1", "--json").stdout)
    assert answer["release"] == "r1"
    assert all(worker["ok"] and worker["result"]["requests"] >= 0 for worker in answer["workers"])
    assert (
        len(
            json.loads(
                daemon.muructl("debug", "status", "plain", "--worker", "1", "--json").stdout
            )["workers"]
        )
        == 1
    )


def test_stacks_show_the_request_each_thread_serves(daemon: Daemon) -> None:
    deploy_plain(daemon, processes=1)
    outcome: list[int] = []

    def slow() -> None:
        connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=60)

        try:
            connection.request("GET", "/sleep?seconds=4")
            outcome.append(connection.getresponse().status)
        finally:
            connection.close()

    thread = threading.Thread(target=slow)
    thread.start()
    time.sleep(0.5)

    try:
        text = daemon.muructl("debug", "stacks", "plain").stdout
    finally:
        thread.join()

    assert outcome == [200]
    assert "serving GET /sleep?seconds=4 for" in text
    assert "plain_wsgi.py" in text and "time.sleep" in text
    assert "MainThread in worker 0:" in text and "muru-app-" in text
    assert "muru-control" not in text

    answer = json.loads(daemon.muructl("debug", "stacks", "plain", "--json").stdout)
    threads = answer["workers"][0]["result"]["threads"]
    assert any(thread["name"] == "MainThread" for thread in threads)


def test_stacks_are_merged_across_workers_showing_the_same_stack(daemon: Daemon) -> None:
    deploy_plain(daemon, processes=2)
    text = daemon.muructl("debug", "stacks", "plain").stdout
    assert "MainThread in 2 workers (0, 1):" in text


def test_inspect_describes_the_interpreter_modules_and_environment(daemon: Daemon) -> None:
    deploy_plain(daemon, processes=1)
    text = daemon.muructl("debug", "inspect", "plain").stdout
    assert "worker 0 (pid" in text and "python 3." in text
    assert "plain in " in text and "(group default, " in text
    assert "modules loaded" in text
    assert "MURU_GROUP=default" in text

    answer = json.loads(daemon.muructl("debug", "inspect", "plain", "--json").stdout)
    result = answer["workers"][0]["result"]
    assert "plain_wsgi" in result["modules"]
    assert result["settings"]["app"] == "plain"
    assert result["descriptors"]["count"] is None or result["descriptors"]["count"] > 3
    assert any(name == "MainThread" for name in (t["name"] for t in result["threads"]))


def test_tasks_list_an_asgi_workers_event_loop(daemon: Daemon) -> None:
    assert daemon.deploy(
        "asgi", support.FIXTURES, "--entrypoint", "plain_asgi:application", "--processes", "1"
    ) == ("muru: release asgi@r1 is ready and serving")
    outcome: list[int] = []

    def slow() -> None:
        connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=60)

        try:
            connection.request("GET", "/stream?count=3&delay=1")
            outcome.append(connection.getresponse().status)
        finally:
            connection.close()

    thread = threading.Thread(target=slow)
    thread.start()
    time.sleep(0.5)

    try:
        text = daemon.muructl("debug", "tasks", "asgi").stdout
    finally:
        thread.join()

    assert "loop lag" in text
    assert "asyncio.sleep" in text or "plain_asgi" in text or "Task-" in text, text
    plain = daemon.muructl("debug", "tasks", "asgi", "--json").stdout
    assert json.loads(plain)["workers"][0]["result"]["lag_ms"] is not None


def test_a_worker_that_cannot_answer_is_reported(daemon: Daemon) -> None:
    deploy_plain(daemon, processes=1)
    pid = json.loads(daemon.get("/info").body)["pid"]
    os.kill(pid, signal.SIGSTOP)

    try:
        started = time.monotonic()
        text = daemon.muructl("debug", "stacks", "plain").stdout
        assert time.monotonic() - started < 15
        assert "did not answer in 5 s; its stacks were written to the log by faulthandler" in text
    finally:
        os.kill(pid, signal.SIGCONT)

    wait_for(lambda: "File " in daemon.muructl("logs", "plain").stdout)
    assert daemon.get("/").status == 200


def test_logging_level_changes_a_logger_for_a_period(daemon: Daemon) -> None:
    assert daemon.deploy(
        "logger", support.FIXTURES, "--entrypoint", "logging_wsgi:application", "--processes", "1"
    ) == ("muru: release logger@r1 is ready and serving")
    assert json.loads(daemon.get("/").body)["noisy_level"] == "NOTSET"

    text = daemon.muructl("logging", "level", "logger", "noisy=DEBUG", "--for", "2s").stdout
    assert "worker 0 (pid" in text and "noisy DEBUG (was NOTSET), for 2s" in text
    assert json.loads(daemon.get("/").body)["noisy_level"] == "DEBUG"
    wait_for(lambda: json.loads(daemon.get("/").body)["noisy_level"] == "NOTSET", timeout=10)

    text = daemon.muructl("logging", "level", "logger", "noisy=DEBUG").stdout
    assert "noisy DEBUG (was NOTSET)" in text
    assert json.loads(daemon.get("/").body)["noisy_level"] == "DEBUG"
    assert "reset noisy" in daemon.muructl("logging", "level", "logger", "--reset").stdout
    assert json.loads(daemon.get("/").body)["noisy_level"] == "NOTSET"

    completed = daemon.muructl("logging", "level", "logger", "noisy=LOUD", check=False)
    assert "is not a logging level" in completed.stdout + completed.stderr


def test_debug_refuses_what_it_cannot_reach(daemon: Daemon) -> None:
    completed = daemon.muructl("debug", "status", "nothing", check=False)
    assert completed.returncode != 0
    completed = daemon.muructl("debug", "status", "plain@r9", check=False)
    assert completed.returncode != 0
