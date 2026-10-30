"""Unit tests for reading a worker from outside (design specification section 11.7)."""

from __future__ import annotations

import textwrap
from pathlib import Path
from typing import Any

import pytest

from muru import _outside

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

    results = _outside.Reader().profile([1], 0.2, 0.01, {1: {"7": "muru-app-0"}})

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
    reader = _outside.Reader()

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
