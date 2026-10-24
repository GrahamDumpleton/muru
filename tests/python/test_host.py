"""Unit tests for the host process helpers."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from muru import host

FIXTURES = Path(__file__).resolve().parents[1] / "fixtures" / "apps"


@pytest.fixture
def fixtures_on_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.syspath_prepend(str(FIXTURES))


def test_resolve_app_default_attribute(fixtures_on_path: None) -> None:
    app = host.resolve_app("plain_wsgi")
    assert callable(app)


def test_resolve_app_explicit_attribute(fixtures_on_path: None) -> None:
    assert host.resolve_app("plain_wsgi:application") is host.resolve_app("plain_wsgi")


def test_resolve_app_rejects_non_callable(fixtures_on_path: None) -> None:
    with pytest.raises(TypeError):
        host.resolve_app("plain_wsgi:HELLO")


def test_resolve_app_rejects_empty_module() -> None:
    with pytest.raises(ValueError, match="module:callable"):
        host.resolve_app(":application")


def test_main_requires_arguments() -> None:
    with pytest.raises(SystemExit) as excinfo:
        host.main([])
    assert excinfo.value.code == 2


def test_main_reports_unloadable_application(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    status = host.main(["--app", "no_such_module:application", "--socket-dir", str(tmp_path)])
    assert status == 1
    assert "cannot load no_such_module:application" in capsys.readouterr().err


def test_describe_status() -> None:
    assert host.describe(0) == "status 0"
    assert "signal" in host.describe(9) or "status" in host.describe(9)


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX only")
def test_python_is_posix() -> None:
    assert hasattr(host.os, "fork")
