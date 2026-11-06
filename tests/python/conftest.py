"""Fixtures shared by the runtime library's unit tests."""

from __future__ import annotations

import sys
import types
from collections.abc import Iterator
from typing import Any

import pytest

import muru


@pytest.fixture
def native(monkeypatch: pytest.MonkeyPatch) -> Iterator[Any]:
    """The runtime's extension, or a stand-in where it is not built.

    The unit tests run without the extension in CI, so code that tells
    the runtime something, such as a thread's name, calls a stand-in
    whose functions do nothing and which a test may replace.
    """

    try:
        from muru import _native
    except ImportError:
        stand_in: Any = types.ModuleType("muru._native")
        stand_in.name_thread = lambda name: None
        stand_in.loop_beat = lambda: None
        stand_in.sync_pool = lambda *arguments: None
        monkeypatch.setitem(sys.modules, "muru._native", stand_in)
        monkeypatch.setattr(muru, "_native", stand_in, raising=False)
        yield stand_in
    else:
        yield _native
