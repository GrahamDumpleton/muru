"""Preload modes and lifecycle hooks (design specification sections 6.1 to 6.3)."""

from __future__ import annotations

import json
import os
import signal
import sysconfig
from collections.abc import Iterator

import pytest
from support import server as support

WORKERS = 2


@pytest.fixture
def hooked() -> Iterator[support.Server]:
    instance = support.start(
        "preload_wsgi:application", workers=WORKERS, args=["--hooks", "hooks_log"]
    )

    try:
        yield instance
    finally:
        instance.stop()


def hook_lines(server: support.Server, name: str) -> list[dict[str, str]]:
    """The fields of each `HOOK name` line, which workers' lines carry after their tag."""

    marker = f"HOOK {name} "
    lines = []

    for line in server.log().splitlines():
        at = line.find(marker)
        if at >= 0:
            lines.append(dict(word.split("=", 1) for word in line[at:].split()[2:]))

    return lines


def import_pids(server: support.Server, count: int) -> set[int]:
    return {json.loads(server.get("/").body)["import_pid"] for _ in range(count)}


def test_hooks_run_in_the_host_and_in_each_worker(hooked: support.Server) -> None:
    host = str(hooked.host_pid())
    for name in ("post_fork", "on_ready"):
        assert hooked.wait_for_log(f"HOOK {name} "), name
    on_load = hook_lines(hooked, "on_load")
    pre_fork = hook_lines(hooked, "pre_fork")
    post_fork = hook_lines(hooked, "post_fork")
    assert [line["pid"] for line in on_load] == [host]
    assert [line["pid"] for line in pre_fork] == [host]
    assert on_load[0]["worker"] == "None"
    assert on_load[0]["app"] == "preload_wsgi"
    assert len(post_fork) == WORKERS
    assert {line["worker"] for line in post_fork} == {"0", "1"}
    assert all(line["pid"] != host for line in post_fork)
    assert len(hook_lines(hooked, "on_ready")) == WORKERS

    # Preloaded: every worker runs the application the host imported.

    assert import_pids(hooked, WORKERS * 8) == {int(host)}
    info = json.loads(hooked.get("/").body)
    assert info["worker_id"] in (0, 1)
    assert info["app"] == "preload_wsgi"

    # The workers run this interpreter fresh, so the build decides, not
    # whether an extension re-enabled the GIL in this process.

    free_threaded = sysconfig.get_config_var("Py_GIL_DISABLED") == 1
    assert info["process_model"] == ("free-threaded" if free_threaded else "threaded")


def test_worker_output_is_tagged_with_the_worker(hooked: support.Server) -> None:
    # Section 11.2: a worker's stderr lines carry its index; its stdout
    # lines name the stream as well. The host's own lines are untagged.

    assert hooked.wait_for_log("HOOK post_fork ")
    log = hooked.log()
    assert any(line.startswith("muru.worker[0]: HOOK post_fork ") for line in log.splitlines())
    assert any(line.startswith("muru.worker[1]: HOOK post_fork ") for line in log.splitlines())
    assert any(line.startswith("HOOK on_load ") for line in log.splitlines())
    hooked.mark_log()
    assert hooked.get("/print").status == 200
    assert hooked.wait_for_log(".stdout: printed by the application")


def test_sigusr1_runs_on_reopen_logs(hooked: support.Server) -> None:
    assert hooked.wait_for_log("HOOK on_ready ")
    pid = hooked.worker_pids()[0]
    hooked.mark_log()
    os.kill(pid, signal.SIGUSR1)
    assert hooked.wait_for_log(f"HOOK on_reopen_logs pid={pid} worker=0")
    assert hooked.get("/").status == 200


def test_drain_and_exit_run_on_shutdown(hooked: support.Server) -> None:
    assert hooked.wait_for_log("HOOK on_ready ")
    hooked.stop()
    assert len(hook_lines(hooked, "on_drain")) == WORKERS
    assert len(hook_lines(hooked, "on_exit")) == WORKERS

    for name in ("on_drain", "on_exit"):
        for line in hook_lines(hooked, name):
            assert line["pid"] != str(hooked.host_pid())


def test_unsafe_import_falls_back_to_per_worker_imports() -> None:
    server = support.start(
        "unsafe_import_wsgi:application", workers=WORKERS, args=["--hooks", "hooks_log"]
    )

    try:
        log = server.log()
        assert "preload: the import of unsafe_import_wsgi left a connected socket" in log
        assert "falling back to per-worker imports" in log
        pids = import_pids(server, WORKERS * 8)
        assert pids == set(server.worker_pids().values())
        assert server.host_pid() not in pids

        # on_load ran once in the host before the check, then on_load and
        # pre_fork run again in each worker, before its post_fork.

        on_load = hook_lines(server, "on_load")
        assert on_load[0] == {
            "pid": str(server.host_pid()),
            "worker": "None",
            "app": "unsafe_import_wsgi",
        }
        assert sorted(line["worker"] for line in on_load[1:]) == ["0", "1"]
        assert sorted(line["worker"] for line in hook_lines(server, "pre_fork")) == ["0", "1"]
    finally:
        server.stop()


def test_preload_true_fails_on_an_unsafe_import() -> None:
    with pytest.raises(RuntimeError) as excinfo:
        support.start("unsafe_import_wsgi:application", args=["--preload", "true"])
    message = str(excinfo.value)
    assert "left a connected socket" in message
    assert "preload is true" in message


def test_preload_false_imports_in_each_worker() -> None:
    server = support.start("preload_wsgi:application", workers=WORKERS, args=["--preload", "false"])

    try:
        assert "preloaded" not in server.log()
        assert import_pids(server, WORKERS * 8) == set(server.worker_pids().values())
    finally:
        server.stop()


def test_failing_on_load_hook_stops_the_host(tmp_path: os.PathLike[str]) -> None:
    with pytest.raises(RuntimeError) as excinfo:
        support.start("preload_wsgi:application", args=["--hooks", "hooks_failing_load"])
    assert "the on_load hook failed" in str(excinfo.value)
    assert "load-boom" in str(excinfo.value)


def test_single_serves_in_the_host_process_without_forking() -> None:
    server = support.start(
        "preload_wsgi:application", workers=3, threads=4, args=["--single", "--hooks", "hooks_log"]
    )

    try:
        assert server.wait_for_log("serving in this process without forking")
        info = json.loads(server.get("/").body)
        host = server.host_pid()
        assert info["pid"] == host
        assert info["import_pid"] == host
        assert info["worker_id"] == 0
        assert info["process_model"] in ("process-per-request", "free-threaded")
        assert server.worker_pids() == {}

        # Hooks still run, in this one process; nothing forks, so pre_fork
        # has no occasion.

        for name in ("on_load", "post_fork", "on_ready"):
            assert [line["pid"] for line in hook_lines(server, name)] == [str(host)], name
        assert hook_lines(server, "pre_fork") == []
    finally:
        server.stop()
