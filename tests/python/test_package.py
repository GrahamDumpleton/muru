"""Smoke tests for the runtime library package."""

from __future__ import annotations

import muru
from muru import host


def test_version_is_a_string() -> None:
    assert isinstance(muru.__version__, str)


def test_host_main_is_not_implemented_yet() -> None:
    assert host.main([]) == 2
