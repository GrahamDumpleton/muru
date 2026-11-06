"""Unit tests for reading a worker from outside (design specification section 11.7)."""

from __future__ import annotations

import textwrap
import threading
from pathlib import Path
from typing import Any

import pytest

from muru import _asgi, _native, _outside

REPORT = textwrap.dedent(
    """\
    def fetch_rows(months):
        return range(months)


    def summarise(rows):
        total = 0

        for row in rows:
            total += row * row % 7

        return total


    def build_report(months):
        return summarise(fetch_rows(months))
    """
)

SCOPES = textwrap.dedent(
    """\
    import functools


    @functools.cache
    @staticmethod
    def decorated():
        return 1


    class Shop:
        def total(self):
            def inner():
                return sum(price for price in self.prices)

            return inner()

        key = lambda item: item.name
    """
)


class Unwinder:
    """An unwinder whose reads fail as listed, then give the trace."""

    def __init__(self, failures: list[Exception], trace: Any) -> None:
        self.failures = list(failures)
        self.trace = trace

    def get_stack_trace(self) -> Any:
        if self.failures:
            raise self.failures.pop(0)

        return self.trace


def fake_proc(tmp_path: Path, pid: int, process: bytes, threads: dict[int, bytes]) -> str:
    """Write the `comm` files Linux shows for a process and its threads.

    Returns the directory that stands for /proc.
    """

    proc = tmp_path / "proc"
    (proc / str(pid)).mkdir(parents=True)
    (proc / str(pid) / "comm").write_bytes(process + b"\n")

    for tid, name in {pid: process, **threads}.items():
        (proc / str(pid) / "task" / str(tid)).mkdir(parents=True)
        (proc / str(pid) / "task" / str(tid) / "comm").write_bytes(name + b"\n")

    return str(proc)


def busy_in_summarise(tmp_path: Path) -> Any:
    """A thread in summarise's loop, called from build_report, innermost first."""

    report = tmp_path / "report.py"
    report.write_text(REPORT)
    return [(7, [(str(report), 9, "summarise"), (str(report), 15, "build_report")])]


def test_a_profile_samples_on_after_a_failed_read(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    failure = RuntimeError("Failed to parse initial frame in chain")
    trace = busy_in_summarise(tmp_path)
    monkeypatch.setattr(_outside, "_unwinder", lambda pid: Unwinder([failure, failure], trace))

    reader = _outside.Reader(str(tmp_path / "proc"))
    results = reader.profile([1], 0.2, 0.01, {1: {"7": "muru-app-0"}})

    # Each function is labelled by the line it starts on, as inside.

    assert results[1]["ok"]
    stacks = results[1]["result"]["stacks"]
    assert list(stacks) == ["muru-app-0;build_report (report.py:14);summarise (report.py:5)"]


def test_a_profile_reports_a_worker_no_read_reached(monkeypatch: pytest.MonkeyPatch) -> None:
    failures: list[Exception] = [RuntimeError("Failed to parse initial frame in chain")] * 1000
    monkeypatch.setattr(_outside, "_unwinder", lambda pid: Unwinder(failures, []))

    results = _outside.Reader().profile([1], 0.1, 0.01, {})

    assert results[1] == {
        "ok": False,
        "error": "could not be read from outside: Failed to parse initial frame in chain",
    }


def test_stacks_are_read_again_after_a_failed_read(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    failure = RuntimeError("Failed to parse initial frame in chain")
    trace = busy_in_summarise(tmp_path)
    monkeypatch.setattr(_outside, "_unwinder", lambda pid: Unwinder([failure], trace))
    reader = _outside.Reader(str(tmp_path / "proc"))

    threads = reader.stacks(1)

    # Stacks keep the line each frame is running.

    assert threads[0][0] == 7
    assert threads[0][1][-1] == {"file": trace[0][1][0][0], "line": 9, "function": "summarise"}
    assert reader.unreadable is None


@pytest.mark.parametrize(
    ("line", "function", "starts"),
    [
        (7, "decorated", 4),
        (13, "Shop.total.<locals>.inner", 12),
        (13, "<genexpr>", 13),
        (15, "Shop.total", 11),
        (17, "<lambda>", 17),
        (17, "Shop", 10),
        (1, "<module>", 1),
        (14, "not_there", 14),
    ],
)
def test_a_function_is_found_where_it_starts(
    tmp_path: Path, line: int, function: str, starts: int
) -> None:
    source = tmp_path / "shop.py"
    source.write_text(SCOPES)

    assert _outside.first_line(str(source), line, function) == starts


def test_a_file_that_cannot_be_read_gives_the_running_line(tmp_path: Path) -> None:
    assert _outside.first_line(str(tmp_path / "gone.py"), 9, "summarise") == 9


def test_threads_read_from_outside_are_named_and_given_their_requests() -> None:
    frames = [{"file": "app.py", "line": 3, "function": "view"}]
    served = {"request_id": "one", "thread": 11, "native": 101}
    awaited = {
        "request_id": "two",
        "thread": None,
        "native": None,
        "sync_threads": [12],
        "sync_native": [102],
    }
    bookkeeping = {
        "active": [served, awaited],
        "names": {"101": "muru-app-0", "102": "muru-sync_0"},
    }

    # Linux knows a thread by its native identifier, and so finds the
    # thread serving a request and one running an ASGI request's
    # synchronous code by theirs.

    threads = [(101, frames, "muru-app-0"), (102, frames, None), (103, frames, None)]
    result = _outside.stacks_result(threads, bookkeeping)

    assert [(thread["name"], thread["request"]) for thread in result["threads"]] == [
        ("muru-app-0", served),
        ("muru-sync_0", awaited),
        ("thread-103", None),
    ]

    # Elsewhere the identifier is Python's.

    result = _outside.stacks_result([(12, frames, None)], {"active": [awaited], "names": {}})
    assert result["threads"][0]["request"] == awaited


def test_a_thread_the_runtime_does_not_name_is_named_by_the_operating_system() -> None:
    frames = [{"file": "app.py", "line": 3, "function": "view"}]
    bookkeeping = {"active": [], "names": {"101": "muru-app-billing-0"}}
    threads = [(101, frames, "muru-app-billin"), (102, frames, "reporter"), (103, frames, None)]

    # The runtime's name is whole where the operating system's is cut.

    result = _outside.stacks_result(threads, bookkeeping)
    assert [thread["name"] for thread in result["threads"]] == [
        "muru-app-billing-0",
        "reporter",
        "thread-103",
    ]


def test_the_names_linux_holds_are_read_by_tid(tmp_path: Path) -> None:
    threads = {
        41: b"muru-loop",
        42: b"python3.15",
        43: b"abcdefghijklmn\xc3",
        44: b"bad\xffbyte",
        45: b"",
    }
    names = _outside.Names(40, fake_proc(tmp_path, 40, b"python3.15", threads))

    # The main thread is the one whose TID is the pid. A thread with the
    # process's name inherited it, a character cut in two by the 15-byte
    # limit is dropped, and a thread that has ended has no name.

    assert names.get(40) == "MainThread"
    assert names.get(41) == "muru-loop"
    assert names.get(42) is None
    assert names.get(43) == "abcdefghijklmn"
    assert names.get(44) == "bad\ufffdbyte"
    assert names.get(45) is None
    assert names.get(46) is None


def test_stacks_carry_the_names_linux_holds(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    report = tmp_path / "report.py"
    report.write_text(REPORT)
    frame = (str(report), 9, "summarise")
    trace = [(40, [frame]), (41, [frame]), (42, [frame])]
    monkeypatch.setattr(_outside, "_unwinder", lambda pid: Unwinder([], trace))
    proc = fake_proc(tmp_path, 40, b"python3.15", {41: b"muru-loop"})

    threads = _outside.Reader(proc).stacks(40)

    assert [(ident, name) for ident, _frames, name in threads] == [
        (40, "MainThread"),
        (41, "muru-loop"),
        (42, None),
    ]


def test_a_profile_names_threads_as_linux_does_asking_once_each(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    report = tmp_path / "report.py"
    report.write_text(REPORT)
    frames = [(str(report), 9, "summarise")]
    trace = [(40, frames), (41, frames), (42, frames)]
    monkeypatch.setattr(_outside, "_unwinder", lambda pid: Unwinder([], trace))
    proc = fake_proc(tmp_path, 40, b"python3.15", {41: b"muru-loop", 42: b"muru-app-0"})
    reads: list[str] = []
    comm = _outside._comm

    def counted(path: str) -> str | None:
        reads.append(path)
        return comm(path)

    monkeypatch.setattr(_outside, "_comm", counted)

    results = _outside.Reader(proc).profile([40], 0.1, 0.01, {40: {"42": "muru-app-0"}})

    # The main thread, whose TID is the pid, is not sampled; the loop
    # thread is named by Linux, read once over every round, and the
    # thread the runtime names is not asked about.

    stacks = results[40]["result"]["stacks"]
    assert sorted(stack.split(";")[0] for stack in stacks) == ["muru-app-0", "muru-loop"]
    assert results[40]["result"]["rounds"] > 1
    assert sorted(path.split("/")[-2] for path in reads) == ["40", "41"]


def test_the_loop_thread_tells_the_runtime_its_name(monkeypatch: pytest.MonkeyPatch) -> None:
    named: list[tuple[str, str]] = []

    def name_thread(name: str) -> None:
        named.append((name, threading.current_thread().name))

    monkeypatch.setattr(_native, "name_thread", name_thread)
    loop = _asgi.Loop(sync_threads=1)
    loop.start()
    loop.stop()

    assert ("muru-loop", "muru-loop") in named
