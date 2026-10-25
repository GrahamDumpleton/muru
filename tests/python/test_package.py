"""Smoke tests for the runtime library package."""

from __future__ import annotations

import pytest

import muru


def test_version_is_a_string() -> None:
    assert isinstance(muru.__version__, str)


def test_is_internal_reads_an_environ_or_a_scope() -> None:
    assert muru.is_internal({}) is False
    assert muru.is_internal({"muru.internal": True, "REDIRECT_URL": "/x"}) is True
    assert muru.is_internal({"type": "http", "extensions": {}}) is False
    assert muru.is_internal({"type": "http", "extensions": {"muru": {"internal": True}}}) is True


def test_native_extension_reports_the_same_version() -> None:
    pytest.importorskip("muru._native")
    from muru import _native

    assert _native.version().replace("-alpha.", "a").replace("-beta.", "b") == muru.__version__ or (
        muru.__version__ == "0+unknown"
    )
