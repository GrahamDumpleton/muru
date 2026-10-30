"""Unit tests for the host process helpers."""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import sys
import textwrap
import time
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


def test_what_a_dead_worker_left_is_stopped_then_killed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1, shutdown_timeout=2.0)
    instance.subreaper = True
    instance.workers = {4243: 0}
    sent: list[tuple[int, int]] = []
    instance.signal = lambda pid, signum: sent.append((pid, signum))  # type: ignore[method-assign]

    # The live worker is not an orphan; the other child the scan finds is.

    monkeypatch.setattr(host, "children_of", lambda pid: {4242, 4243})
    instance.adopt_orphans(0, 4000)
    assert list(instance.orphans) == [4242]
    assert instance.orphans[4242].left_by == 0

    # Section 6.10: the shutdown timeout to exit on its own, then SIGTERM,
    # then as long again before SIGKILL.

    now = time.monotonic()
    instance.advance_orphans(now + 1.0)
    assert sent == []
    instance.advance_orphans(now + 2.1)
    assert sent == [(4242, signal.SIGTERM)]
    instance.advance_orphans(now + 3.0)
    assert sent == [(4242, signal.SIGTERM)]
    instance.advance_orphans(now + 4.2)
    assert sent == [(4242, signal.SIGTERM), (4242, signal.SIGKILL)]

    # Adopting again finds nothing new, and a host that is no subreaper
    # adopts nothing at all.

    instance.adopt_orphans(0, 4000)
    assert list(instance.orphans) == [4242]
    instance.subreaper = False
    monkeypatch.setattr(host, "children_of", lambda pid: {4242, 4243, 4244})
    instance.adopt_orphans(0, 4000)
    assert list(instance.orphans) == [4242]


def test_become_subreaper_is_linux_only() -> None:
    if sys.platform == "linux":
        pytest.skip("on Linux the subreaper takes; the test below covers it")
    assert host.become_subreaper() is False
    assert host.children_of(os.getpid()) == set()


@pytest.mark.requires_linux
def test_a_subreaper_sees_orphaned_grandchildren_as_its_children() -> None:
    # In a process of its own, since the subreaper setting is for life.

    script = textwrap.dedent(
        """
        import os
        import signal
        import time

        from muru import host

        assert host.become_subreaper()
        read_end, write_end = os.pipe()
        pid = os.fork()

        if pid == 0:
            grandchild = os.fork()

            if grandchild == 0:
                time.sleep(60)
                os._exit(0)

            os.write(write_end, str(grandchild).encode())
            os._exit(0)

        os.waitpid(pid, 0)
        grandchild = int(os.read(read_end, 32))
        deadline = time.monotonic() + 5

        while grandchild not in host.children_of(os.getpid()) and time.monotonic() < deadline:
            time.sleep(0.01)

        print("adopted", grandchild in host.children_of(os.getpid()))
        os.kill(grandchild, signal.SIGTERM)
        print("collected", os.waitpid(grandchild, 0)[0] == grandchild)
        print("gone", grandchild not in host.children_of(os.getpid()))
        """
    )
    result = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
        check=False,
        env={**os.environ, "PYTHONPATH": str(Path(host.__file__).resolve().parents[1])},
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.split() == ["adopted", "True", "collected", "True", "gone", "True"]
