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
        "interfaces": {},
    }

    # The interfaces the host settled go with it, the default mount's
    # under the empty name; a mount its workers settle is left out.

    context = host.Context(app="plain_wsgi")
    settled = host.Mount.parse("ws=/ws=plain_asgi:application", context, 1)
    unsettled = host.Mount.parse("other=/other=plain_wsgi:application", context, 1)
    release.resolved = "wsgi"
    settled.release.resolved = "asgi"
    instance.mounts = [settled, unsettled]
    assert instance.workers_message()["interfaces"] == {"": "wsgi", "ws": "asgi"}


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


def test_a_restart_is_refused_for_a_worker_the_host_cannot_replace(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1)
    answers: list[dict[str, object]] = []
    instance.send_to_supervisor = answers.append  # type: ignore[method-assign]

    instance.take_request({"id": 1, "op": "restart", "args": {}})
    assert answers[-1] == {
        "type": "response",
        "id": 1,
        "workers": [],
        "error": "a worker index is needed",
    }

    instance.take_request({"id": 2, "op": "restart", "worker": 3, "args": {"force": True}})
    assert answers[-1]["error"] == "no worker 3"


def test_arguments_are_checked_against_the_interface(monkeypatch: pytest.MonkeyPatch) -> None:
    def wsgi(environ: object, start_response: object) -> None: ...
    def factory() -> None: ...
    def factory_with_defaults(config: object = None) -> None: ...
    def three(scope: object, receive: object, send: object) -> None: ...
    def any_args(*args: object, **kwargs: object) -> None: ...
    def keyword(environ: object, start_response: object, *, extra: object) -> None: ...

    async def asgi(scope: object, receive: object, send: object) -> None: ...

    async def two_async(scope: object, receive: object) -> None: ...

    assert host.resolve_interface("m:wsgi", wsgi, "auto") == "wsgi"
    assert host.resolve_interface("m:asgi", asgi, "auto") == "asgi"
    assert host.resolve_interface("m:any", any_args, "auto") == "wsgi"
    assert host.resolve_interface("m:three", three, "asgi") == "asgi"

    with pytest.raises(RuntimeError, match="set factory = true"):
        host.resolve_interface("m:factory", factory, "auto")

    with pytest.raises(RuntimeError, match="set factory = true"):
        host.resolve_interface("m:factory", factory_with_defaults, "auto")

    with pytest.raises(RuntimeError, match="as an ASGI application does, but is not a coroutine"):
        host.resolve_interface("m:three", three, "auto")

    with pytest.raises(RuntimeError, match="declared ASGI but takes 2 arguments"):
        host.resolve_interface("m:wsgi", wsgi, "asgi")

    with pytest.raises(RuntimeError, match="declared WSGI but takes 3 arguments"):
        host.resolve_interface("m:three", three, "wsgi")

    with pytest.raises(RuntimeError, match="coroutine function, so it cannot be served as WSGI"):
        host.resolve_interface("m:two", two_async, "auto")

    with pytest.raises(RuntimeError, match="keyword-only parameter 'extra'"):
        host.resolve_interface("m:keyword", keyword, "auto")

    # What Python cannot read the signature of is taken as it is.

    monkeypatch.setattr(host.inspect, "signature", lambda app: (_ for _ in ()).throw(ValueError()))
    assert host.resolve_interface("m:factory", factory, "auto") == "wsgi"
