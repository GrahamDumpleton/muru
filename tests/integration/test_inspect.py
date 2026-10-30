"""Worker inspection (design specification section 11.7): `muructl debug`
asks a release's workers for their status, Python stacks with the request
each thread serves, asyncio tasks and what they have loaded, through the
host's control connection, and `muructl logging level` changes a logger's
level for a period. A worker that cannot answer is reported as such."""

from __future__ import annotations

import http.client
import json
import os
import pstats
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Callable
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon
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


def readable_from_outside() -> bool:
    """Whether this user can read a child interpreter's stacks with PEP
    768, as a host reads its workers': Python 3.14 or later, and root on
    macOS."""

    try:
        import _remote_debugging  # type: ignore[import-not-found]
    except ImportError:
        return False

    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])

    try:
        time.sleep(0.5)
        _remote_debugging.RemoteUnwinder(child.pid, all_threads=True).get_stack_trace()
        return True
    except (OSError, RuntimeError):
        return False
    finally:
        child.kill()
        child.wait()


def test_a_worker_that_cannot_answer_is_reported(daemon: Daemon) -> None:
    deploy_plain(daemon, processes=1)
    daemon.wait_for_routing("plain")
    pid = json.loads(daemon.get("/info").body)["pid"]
    os.kill(pid, signal.SIGSTOP)

    try:
        started = time.monotonic()
        asked = daemon.muructl("debug", "stacks", "plain", "--json", check=False)
        assert asked.returncode == 0, (
            f"{asked.stdout}{asked.stderr}\n--- events\n"
            f"{daemon.muructl('events').stdout}\n--- daemon log\n{daemon.log()[-4000:]}"
        )
        worker = json.loads(asked.stdout)["workers"][0]
        assert time.monotonic() - started < 15
    finally:
        os.kill(pid, signal.SIGCONT)

    # Read from outside, a stopped worker's stacks are there all the same;
    # otherwise faulthandler writes them to the log.

    if readable_from_outside():
        assert worker["ok"] and worker["result"]["outside"], worker
        assert any(thread["name"] == "MainThread" for thread in worker["result"]["threads"])
    else:
        error = worker["error"]
        assert "did not answer in 5 s; its stacks were written to the log by faulthandler" in error
        wait_for(lambda: "File " in daemon.muructl("logs", "plain").stdout)

    assert daemon.get("/").status == 200


def test_a_stopped_worker_does_not_hold_up_the_daemon(daemon: Daemon) -> None:
    deploy_plain(daemon, processes=1)
    daemon.wait_for_routing("plain")
    pid = json.loads(daemon.get("/info").body)["pid"]
    os.kill(pid, signal.SIGSTOP)

    # A snapshot names the stopped worker, which accepts the edge's
    # connection and never answers HELLO: the edge gives up on it and
    # applies the snapshot without it, the daemon goes on answering, and
    # the worker is named and replaced.

    try:
        started = time.monotonic()
        daemon.muructl("maintenance", "on", "plain")
        assert time.monotonic() - started < 15

        started = time.monotonic()
        assert "plain@r1" in daemon.muructl("release", "list", "plain").stdout
        daemon.muructl("debug", "status", "plain")
        assert time.monotonic() - started < 5
        log = daemon.muructl("events").stdout
        assert "worker.unreachable" in daemon.events(), log
        assert "did not answer HELLO within 5 s" in log, log
        wait_for(lambda: "worker.restarted" in daemon.events())
    finally:
        try:
            os.kill(pid, signal.SIGCONT)
        except ProcessLookupError:
            pass

    daemon.muructl("maintenance", "off", "plain")
    wait_for(lambda: daemon.get("/").status == 200)
    assert json.loads(daemon.get("/info").body)["pid"] != pid


def test_stacks_are_read_from_outside_a_worker_holding_the_interpreter(daemon: Daemon) -> None:
    if not readable_from_outside():
        pytest.skip("reading a worker from outside needs Python 3.14 or later, and root on macOS")

    deploy_plain(daemon, processes=1)
    threads = in_background(daemon, "/hold?seconds=4")

    try:
        started = time.monotonic()
        answer = json.loads(daemon.muructl("debug", "stacks", "plain", "--json").stdout)
        elapsed = time.monotonic() - started
        text = daemon.muructl("debug", "stacks", "plain").stdout
    finally:
        for thread in threads:
            thread.join()

    # The worker's own control thread could not have answered: the
    # interpreter is held. The stacks come from outside at once, and the
    # runtime's bookkeeping, kept without the interpreter, names the
    # request.

    assert elapsed < 4, elapsed
    result = answer["workers"][0]["result"]
    assert result["outside"]
    holding = next(thread for thread in result["threads"] if thread["request"])
    assert holding["request"]["path"] == "/hold?seconds=4"
    assert holding["name"].startswith("muru-app-")
    assert any(frame["function"] == "hold_the_interpreter" for frame in holding["frames"])
    assert "serving GET /hold?seconds=4 for" in text


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


def in_background(daemon: Daemon, *paths: str) -> list[threading.Thread]:
    """Requests running in threads, one for each path."""

    def fetch(path: str) -> None:
        connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=60)

        try:
            connection.request("GET", path)
            connection.getresponse().read()
        finally:
            connection.close()

    threads = [threading.Thread(target=fetch, args=(path,)) for path in paths]

    for thread in threads:
        thread.start()

    time.sleep(0.3)
    return threads


def test_profile_samples_the_stacks_running_python(daemon: Daemon, tmp_path: Path) -> None:
    deploy_plain(daemon, processes=1)
    threads = in_background(daemon, "/busy?seconds=3")
    output = tmp_path / "profile.txt"

    try:
        completed = daemon.muructl(
            "debug", "profile", "plain", "--for", "1s", "--output", str(output)
        )
    finally:
        for thread in threads:
            thread.join()

    lines = output.read_text().splitlines()
    assert lines, completed.stderr
    stack, count = lines[0].rsplit(" ", 1)
    assert int(count) > 10
    assert stack.startswith("muru-app-")
    assert "application (plain_wsgi.py:" in stack
    assert "samples of running Python code" in completed.stderr
    assert "debug.profile" in daemon.events()


def test_profile_samples_the_requests_chosen_and_writes_pstats(
    daemon: Daemon, tmp_path: Path
) -> None:
    deploy_plain(daemon, processes=1)
    threads = in_background(daemon, "/sleep?seconds=4", "/busy?seconds=2")
    output = tmp_path / "chosen.txt"
    started = time.monotonic()

    try:
        completed = daemon.muructl(
            "debug",
            "profile",
            "plain",
            "--for",
            "20s",
            "--path",
            "/busy",
            "--requests",
            "1",
            "--output",
            str(output),
        )
        elapsed = time.monotonic() - started
    finally:
        for thread in threads:
            thread.join()

    # Only the thread serving /busy is sampled, and the profile ends with
    # its request, long before the period.

    lines = output.read_text().splitlines()
    assert lines, completed.stderr
    assert len({line.split(";", 1)[0] for line in lines}) == 1, lines
    assert "1 request sampled to the end" in completed.stderr
    assert elapsed < 10, elapsed

    threads = in_background(daemon, "/busy?seconds=2")
    written = tmp_path / "profile.pstats"

    try:
        completed = daemon.muructl(
            "debug",
            "profile",
            "plain",
            "--for",
            "1s",
            "--format",
            "pstats",
            "--output",
            str(written),
        )
    finally:
        for thread in threads:
            thread.join()

    stats = pstats.Stats(str(written))
    functions = {(file, name) for file, _, name in stats.stats}  # type: ignore[attr-defined]
    assert ("plain_wsgi.py", "application") in functions, functions
    assert "pstats written to" in completed.stderr

    refused = daemon.muructl("debug", "profile", "plain", "--format", "pstats", check=False)
    assert refused.returncode != 0
    assert "needs --output" in refused.stderr


def test_trace_counts_calls_and_refuses_a_busy_worker(daemon: Daemon) -> None:
    if sys.version_info < (3, 12):
        support.unavailable("trace needs Python 3.12 or later, for sys.monitoring")

    deploy_plain(daemon, processes=1)
    stop = threading.Event()

    def requests() -> None:
        while not stop.is_set():
            daemon.get("/")

    caller = threading.Thread(target=requests)
    caller.start()

    try:
        text = daemon.muructl("debug", "trace", "plain", "--for", "1s").stdout
    finally:
        stop.set()
        caller.join()

    assert "total ms" in text
    assert "application (plain_wsgi.py:" in text

    # Both of the worker's threads busy: refused unless forced.

    threads = in_background(daemon, "/sleep?seconds=3", "/sleep?seconds=3")

    try:
        refused = daemon.muructl("debug", "trace", "plain", "--for", "1s").stdout
        forced = daemon.muructl("debug", "trace", "plain", "--for", "1s", "--force").stdout
    finally:
        for thread in threads:
            thread.join()

    assert "the worker is busy (2 of 2 slots)" in refused
    assert "total ms" in forced


def test_memory_reports_objects_and_traces_allocations(daemon: Daemon) -> None:
    deploy_plain(daemon, processes=1)
    text = daemon.muructl("debug", "memory", "plain").stdout
    assert "gc counts" in text
    assert "objects:" in text and " dict" in text

    stop = threading.Event()

    def grow() -> None:
        while not stop.is_set():
            daemon.get("/grow?mb=1")
            time.sleep(0.1)

    grower = threading.Thread(target=grow)
    grower.start()

    try:
        traced = daemon.muructl("debug", "memory", "plain", "--trace", "--for", "1s").stdout
    finally:
        stop.set()
        grower.join()

    assert "allocated over the period:" in traced
    assert "plain_wsgi.py" in traced


def test_memory_compares_with_a_snapshot_kept_between_two_points(daemon: Daemon) -> None:
    deploy_plain(daemon, processes=1)
    kept = daemon.muructl("debug", "memory", "plain", "--snapshot").stdout
    assert "snapshot kept; tracemalloc holds" in kept

    for _ in range(3):
        daemon.get("/grow?mb=1")

    compared = daemon.muructl("debug", "memory", "plain", "--diff").stdout
    assert "allocated since the snapshot:" in compared
    assert "plain_wsgi.py" in compared

    # Kept until dropped; then there is nothing to compare with.

    assert "plain_wsgi.py" in daemon.muructl("debug", "memory", "plain", "--diff").stdout
    assert "snapshot dropped" in daemon.muructl("debug", "memory", "plain", "--drop").stdout
    refused = daemon.muructl("debug", "memory", "plain", "--diff").stdout
    assert "no snapshot is kept; take one with --snapshot" in refused


def test_a_traced_worker_leaves_routing_while_the_trace_lasts(daemon: Daemon) -> None:
    if sys.version_info < (3, 12):
        support.unavailable("trace needs Python 3.12 or later, for sys.monitoring")

    deploy_plain(daemon, processes=2)

    def serving() -> set[int]:
        return {json.loads(daemon.get("/info").body)["pid"] for _ in range(20)}

    wait_for(lambda: len(serving()) == 2)
    tracing = threading.Thread(
        target=daemon.muructl, args=("debug", "trace", "plain", "--worker", "0", "--for", "4s")
    )
    tracing.start()

    try:
        wait_for(lambda: len(serving()) == 1, timeout=3)
        during = serving()
    finally:
        tracing.join()

    assert len(during) == 1
    wait_for(lambda: len(serving()) == 2)
    log = daemon.muructl("logs", "plain").stdout
    assert "leaves routing while it is traced" in log
    assert "serves again: its trace has ended" in log


def test_exec_is_off_unless_the_daemon_allows_it(daemon: Daemon, tmp_path: Path) -> None:
    deploy_plain(daemon, processes=1)
    refused = daemon.muructl("debug", "exec", "plain", "--", "1 + 1", check=False)
    assert refused.returncode != 0
    assert "debug exec is off" in refused.stderr

    (tmp_path / "allowed").mkdir()
    allowed = start_daemon(tmp_path / "allowed", config_extra="[daemon]\ndebug_exec = true\n")

    try:
        deploy_plain(allowed, processes=1)
        text = allowed.muructl(
            "debug", "exec", "plain", "--", "import os; print('from', os.getpid()); 6 * 7"
        ).stdout
        pid = json.loads(allowed.get("/info").body)["pid"]
        assert f"from {pid}" in text
        assert "42" in text
        failed = allowed.muructl("debug", "exec", "plain", "--", "1 / 0").stdout
        assert "ZeroDivisionError" in failed
        assert "debug.exec" in allowed.events()
    finally:
        allowed.stop()
