"""Unit tests for reading a worker from outside (design specification section 11.7)."""

from __future__ import annotations

from typing import Any

import pytest

from muru import _outside

# A thread busy in summarise, called from build_report, as the unwinder
# gives it: the thread's identifier and its frames, innermost first.

TRACE = [(7, [("/app/report.py", 14, "summarise"), ("/app/report.py", 21, "build_report")])]


class Unwinder:
    """An unwinder whose reads fail as listed, then succeed."""

    def __init__(self, failures: list[Exception]) -> None:
        self.failures = list(failures)

    def get_stack_trace(self) -> Any:
        if self.failures:
            raise self.failures.pop(0)

        return TRACE


def test_a_profile_samples_on_after_a_failed_read(monkeypatch: pytest.MonkeyPatch) -> None:
    failure = RuntimeError("Failed to parse initial frame in chain")
    monkeypatch.setattr(_outside, "_unwinder", lambda pid: Unwinder([failure, failure]))

    results = _outside.Reader().profile([1], 0.2, 0.01, {1: {"7": "muru-app-0"}})

    assert results[1]["ok"]
    stacks = results[1]["result"]["stacks"]
    assert list(stacks) == ["muru-app-0;build_report (report.py:21);summarise (report.py:14)"]


def test_a_profile_reports_a_worker_no_read_reached(monkeypatch: pytest.MonkeyPatch) -> None:
    failures: list[Exception] = [RuntimeError("Failed to parse initial frame in chain")] * 1000
    monkeypatch.setattr(_outside, "_unwinder", lambda pid: Unwinder(failures))

    results = _outside.Reader().profile([1], 0.1, 0.01, {})

    assert results[1] == {
        "ok": False,
        "error": "could not be read from outside: Failed to parse initial frame in chain",
    }


def test_stacks_are_read_again_after_a_failed_read(monkeypatch: pytest.MonkeyPatch) -> None:
    failure = RuntimeError("Failed to parse initial frame in chain")
    monkeypatch.setattr(_outside, "_unwinder", lambda pid: Unwinder([failure]))
    reader = _outside.Reader()

    threads = reader.stacks(1)

    assert threads[0][0] == 7
    assert threads[0][1][-1]["function"] == "summarise"
    assert reader.unreadable is None
