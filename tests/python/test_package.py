"""Smoke tests for the runtime library package."""

from __future__ import annotations

import pytest

import muru


def test_version_is_a_string() -> None:
    assert isinstance(muru.__version__, str)


def test_native_extension_reports_the_same_version() -> None:
    pytest.importorskip("muru._native")
    from muru import _native

    assert _native.version().replace("-alpha.", "a").replace("-beta.", "b") == muru.__version__ or (
        muru.__version__ == "0+unknown"
    )
