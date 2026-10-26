"""Unit tests for debugger sessions (design specification section 11.7)."""

from __future__ import annotations

import sys
import time

import pytest

from muru import _debug


def output_until(session_id: int, text: str, wanted: str) -> str:
    """Send `text`, then gather what pdb writes until `wanted` shows."""

    seen = _debug.exchange(session_id, text, 2.0)["output"]
    deadline = time.monotonic() + 10

    while wanted not in seen and time.monotonic() < deadline:
        seen += _debug.exchange(session_id, "", 0.5)["output"]

    return seen


def test_a_session_with_no_breakpoint_has_a_thread_of_its_own() -> None:
    assert _debug.waiting() == {"waiting": []}
    opened = _debug.attach()
    assert opened["where"] == "a thread of its own"
    prompt = opened["output"]

    if "(Pdb)" not in prompt:
        prompt += output_until(opened["session"], "", "(Pdb)")

    assert "(Pdb)" in prompt

    assert "42" in output_until(opened["session"], "p 6 * 7", "42")
    assert _debug.detach(opened["session"])["ended"]


def test_install_leaves_a_hook_named_in_the_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sys, "breakpointhook", sys.__breakpointhook__)
    monkeypatch.setenv("PYTHONBREAKPOINT", "pdb.set_trace")
    _debug.install(0, None)
    assert sys.breakpointhook is sys.__breakpointhook__

    monkeypatch.delenv("PYTHONBREAKPOINT")
    _debug.install(0, None)
    assert sys.breakpointhook is _debug.breakpoint_hook
