"""Shared pytest configuration: skip Linux-only tests on other platforms,
and remove the short directories the tests made under /tmp."""

from __future__ import annotations

import sys

import pytest
from support import server


def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    """Skip tests marked requires_linux when not running on Linux."""

    if sys.platform.startswith("linux"):
        return

    skip = pytest.mark.skip(reason="needs Linux-only features")

    for item in items:
        if "requires_linux" in item.keywords:
            item.add_marker(skip)


def pytest_sessionfinish(session: pytest.Session, exitstatus: int) -> None:
    """Remove the directories support.server.short_dir made."""

    server.remove_short_dirs()
