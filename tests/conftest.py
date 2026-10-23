"""Shared pytest configuration: skip Linux-only tests on other platforms."""

from __future__ import annotations

import sys

import pytest


def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    """Skip tests marked requires_linux when not running on Linux."""

    if sys.platform.startswith("linux"):
        return

    skip = pytest.mark.skip(reason="needs Linux-only features")
    for item in items:
        if "requires_linux" in item.keywords:
            item.add_marker(skip)
