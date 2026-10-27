"""Unit tests for the host process helpers."""

from __future__ import annotations

import argparse
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


def test_socket_paths_carry_the_generation_of_a_replacement(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=2, threads=1)
    assert instance.socket_path(1) == str(tmp_path / "worker-1.sock")
    instance.generation[1] = 2
    assert instance.socket_path(1) == str(tmp_path / "worker-1-g2.sock")
    assert instance.socket_path(0) == str(tmp_path / "worker-0.sock")


def test_workers_message_lists_the_routed_sockets_by_group(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    api = host.Group("api", 1, 1, 1, 1)
    instance = host.Host(release, str(tmp_path), workers=2, threads=1, groups=[api])
    instance.routed = {0: "/a", 1: None, 2: "/api-0"}
    instance.version = 3
    assert instance.workers_message() == {
        "type": "workers",
        "version": 3,
        "sockets": ["/a"],
        "groups": [{"name": "api", "sockets": ["/api-0"]}],
    }


def test_a_mount_takes_threads_of_its_own() -> None:
    context = host.Context(app="shop")
    mount = host.Mount.parse("reports=/reports=shop:reports=wsgi=2,6", context, 4)
    assert (mount.name, mount.path, mount.threads) == ("reports", "/reports", (2, 6))
    assert host.Mount.parse("reports=/reports=shop:reports", context, 4).threads is None

    for text in (
        "reports=/reports=shop:reports=wsgi=2",
        "reports=/reports=shop:reports=wsgi=3,2",
        "reports=/reports=shop:reports=wsgi=0,0",
        "reports=/reports=shop:reports=wsgi=-1,2",
    ):
        with pytest.raises(argparse.ArgumentTypeError):
            host.Mount.parse(text, context, 4)
