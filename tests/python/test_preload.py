"""Unit tests for the import check of section 6.2."""

from __future__ import annotations

import os
import subprocess
import sys
import threading
from collections.abc import Iterator
from pathlib import Path

import pytest

from muru import preload


@pytest.fixture
def modules(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    monkeypatch.syspath_prepend(str(tmp_path))
    before = set(sys.modules)
    yield tmp_path
    for name in set(sys.modules) - before:
        module = sys.modules.pop(name)
        cleanup = getattr(module, "cleanup", None)
        if callable(cleanup):
            cleanup()


def write_module(directory: Path, name: str, body: str) -> None:
    (directory / f"{name}.py").write_text(body)


def test_thread_count_sees_a_new_thread() -> None:
    before = preload.thread_count()
    release = threading.Event()
    thread = threading.Thread(target=release.wait)
    thread.start()
    try:
        assert preload.thread_count() == before + 1
    finally:
        release.set()
        thread.join()


def test_child_pids_sees_a_child_process() -> None:
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
    try:
        assert child.pid in preload.child_pids()
    finally:
        child.kill()
        child.wait()
    assert child.pid not in preload.child_pids()


def test_clean_import_has_no_findings(modules: Path) -> None:
    write_module(modules, "clean_mod", "import json\nVALUE = json.dumps({})\n")
    with preload.ImportRecorder() as recorder:
        recorder.import_module("clean_mod")
    assert recorder.findings() == []


def test_regular_file_is_not_reported(modules: Path) -> None:
    path = modules / "data.txt"
    path.write_text("x")
    write_module(
        modules,
        "file_mod",
        f"HANDLE = open({str(path)!r})\n\ndef cleanup():\n    HANDLE.close()\n",
    )
    with preload.ImportRecorder() as recorder:
        recorder.import_module("file_mod")
    assert recorder.findings() == []


def test_connected_socket_is_attributed_to_its_module(modules: Path) -> None:
    write_module(
        modules,
        "socket_mod",
        "import socket\nPAIR = socket.socketpair()\n\n"
        "def cleanup():\n    PAIR[0].close()\n    PAIR[1].close()\n",
    )
    write_module(modules, "outer_mod", "import socket_mod\n")
    with preload.ImportRecorder() as recorder:
        recorder.import_module("outer_mod")
    findings = recorder.findings()
    assert findings, "a socketpair left open was not reported"
    assert all(finding.kind == "socket" for finding in findings)
    assert all(finding.module == "socket_mod" for finding in findings)
    assert "connected socket" in findings[0].describe()
    assert "pre_fork" in findings[0].advice


def test_unconnected_udp_socket_is_not_reported(modules: Path) -> None:
    write_module(
        modules,
        "udp_mod",
        "import socket\nSOCK = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)\n\n"
        "def cleanup():\n    SOCK.close()\n",
    )
    with preload.ImportRecorder() as recorder:
        recorder.import_module("udp_mod")
    assert recorder.findings() == []


def test_thread_is_attributed_to_its_module(modules: Path) -> None:
    write_module(
        modules,
        "thread_mod",
        "import threading\nSTOP = threading.Event()\n"
        "THREAD = threading.Thread(target=STOP.wait)\nTHREAD.start()\n\n"
        "def cleanup():\n    STOP.set()\n    THREAD.join()\n",
    )
    with preload.ImportRecorder() as recorder:
        recorder.import_module("thread_mod")
    findings = recorder.findings()
    assert [finding.kind for finding in findings] == ["thread"]
    assert findings[0].module == "thread_mod"
    assert "post_fork" in findings[0].advice


def test_child_process_and_its_pipes_are_reported(modules: Path) -> None:
    write_module(
        modules,
        "child_mod",
        "import subprocess, sys\n"
        "CHILD = subprocess.Popen([sys.executable, '-c', 'import sys; sys.stdin.read()'],"
        " stdin=subprocess.PIPE)\n\n"
        "def cleanup():\n    CHILD.kill()\n    CHILD.wait()\n",
    )
    with preload.ImportRecorder() as recorder:
        recorder.import_module("child_mod")
    kinds = {finding.kind for finding in recorder.findings()}
    assert kinds == {"child", "pipe"}


def test_import_statements_are_attributed(modules: Path) -> None:
    write_module(
        modules,
        "inner_socket_mod",
        "import socket\nPAIR = socket.socketpair()\n\n"
        "def cleanup():\n    PAIR[0].close()\n    PAIR[1].close()\n",
    )
    write_module(modules, "via_statement_mod", "import inner_socket_mod\n")
    with preload.ImportRecorder() as recorder:
        # The outer module is imported through importlib, the inner one by
        # its import statement, which the wrapped __import__ sees.

        recorder.import_module("via_statement_mod")
    assert {finding.module for finding in recorder.findings()} == {"inner_socket_mod"}


def test_recorder_restores_import() -> None:
    import builtins

    original = builtins.__import__
    with preload.ImportRecorder():
        assert builtins.__import__ is not original
    assert builtins.__import__ is original


def test_socket_closed_before_the_end_is_not_reported(modules: Path) -> None:
    write_module(
        modules,
        "closed_mod",
        "import socket\nPAIR = socket.socketpair()\nPAIR[0].close()\nPAIR[1].close()\n",
    )
    with preload.ImportRecorder() as recorder:
        recorder.import_module("closed_mod")
    assert recorder.findings() == []


def test_descriptor_reuse_is_compared_by_identity(modules: Path) -> None:
    # A descriptor number that existed before the import but now refers to
    # a connected socket is a finding, not a survivor of the baseline.

    placeholder = os.open(os.devnull, os.O_RDONLY)
    write_module(
        modules,
        "reuse_mod",
        f"import os, socket\nos.close({placeholder})\nPAIR = socket.socketpair()\n\n"
        "def cleanup():\n    PAIR[0].close()\n    PAIR[1].close()\n",
    )
    with preload.ImportRecorder() as recorder:
        recorder.import_module("reuse_mod")
    assert {finding.kind for finding in recorder.findings()} == {"socket"}


def test_peer_name_formats_addresses() -> None:
    assert preload._format_peer(("127.0.0.1", 5432)) == "127.0.0.1:5432"
    assert preload._format_peer(("::1", 5432)) == "[::1]:5432"
    assert preload._format_peer("/tmp/x.sock") == "/tmp/x.sock"
    assert preload._format_peer("") == "an unnamed peer"
