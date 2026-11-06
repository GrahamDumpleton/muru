"""Handing a request between WSGI and ASGI (design specification section 6.14).

A WSGI application with handoff = "asgi" continues a request as an ASGI
application on the worker's loop: an HTTP request with the body it has
not read, the scope state it filled and its context, or a WebSocket,
which a WSGI path receives as one only when its mount allows the
handoff. Without the setting the environ key is still there and its
handoff raises HandoffNotEnabled, naming the setting (ADR 0042).
"""

from __future__ import annotations

import contextlib
import http.client
import json
import re
import sys
import threading
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon
from websockets.exceptions import ConnectionClosed, InvalidStatus
from websockets.sync.client import connect

HANDOFF = support.ROOT / "tests" / "fixtures" / "projects" / "handoff"
APPS = support.ROOT / "tests" / "fixtures" / "apps"


@pytest.fixture(scope="module")
def server() -> Iterator[support.Server]:
    instance = support.start(str(HANDOFF), workers=None, threads=None, chdir=None)

    try:
        yield instance
    finally:
        instance.stop()


def test_an_http_request_continues_with_the_body_it_left(server: support.Server) -> None:
    response = server.post("/body", b"hello world, the rest")
    assert response.status == 200, response.body
    assert json.loads(response.body) == {
        "type": "http",
        "path": "/body",
        "body": " world, the rest",
        "state": {"first": "hello"},
        "context": "set in wsgi",
    }

    # A body still arriving streams on to the application it was handed to.

    def pieces() -> Iterator[bytes]:
        yield b"abc"
        time.sleep(0.2)
        yield b"defgh"
        time.sleep(0.2)
        yield b"ij"

    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=10)

    try:
        connection.request(
            "POST",
            "/body",
            body=pieces(),
            headers={"Transfer-Encoding": "chunked"},
            encode_chunked=True,
        )
        streamed = connection.getresponse()
        assert streamed.status == 200
        assert json.loads(streamed.read())["body"] == "fghij"

        # The connection serves the next request as usual.

        connection.request("GET", "/")
        after = connection.getresponse()
        assert (after.status, after.read()) == (200, b"wsgi False\n")
    finally:
        connection.close()


def test_a_websocket_is_handed_to_asgi(server: support.Server) -> None:
    with connect(f"ws://127.0.0.1:{server.port}/ws", open_timeout=10) as socket_:
        assert socket_.recv(timeout=10) == "hello from wsgi"
        socket_.send("ping")
        assert socket_.recv(timeout=10) == "ping"
        socket_.send(b"\x00\x01")
        assert socket_.recv(timeout=10) == b"\x00\x01"

    assert server.wait_for_log("101 GET /ws")

    # Without an upgrade the same view sees no WebSocket.

    plain = server.get("/ws")
    assert (plain.status, plain.body) == (426, b"websocket required\n")


def test_declining_an_upgrade_is_an_ordinary_response(server: support.Server) -> None:
    with pytest.raises(InvalidStatus) as declined:
        connect(f"ws://127.0.0.1:{server.port}/ws-decline", open_timeout=10)

    assert declined.value.response.status_code == 403
    assert declined.value.response.body == b"declined True\n"


def test_a_mount_without_the_setting_raises_and_takes_no_websocket(
    server: support.Server,
) -> None:
    response = server.get("/plain/x")
    assert response.status == 409
    assert response.body == (
        b"HandoffNotEnabled: mount plain does not enable handing requests to ASGI; "
        b'set handoff = "asgi" on the mount\n'
    )

    # An upgrade to a WSGI mount without the handoff is an ordinary request.

    with pytest.raises(InvalidStatus) as refused:
        connect(f"ws://127.0.0.1:{server.port}/plain/ws", open_timeout=10)

    assert refused.value.response.status_code == 409


def test_the_wsgi_response_after_a_handoff_is_ignored(server: support.Server) -> None:
    discarded = server.get("/discarded")
    assert discarded.status == 200
    assert json.loads(discarded.body)["path"] == "/discarded"

    server.mark_log()
    twice = server.get("/twice")
    assert twice.status == 200
    assert server.wait_for_log("second handoff: the request has already been handed off")

    # Once output has left, the request stays WSGI.

    written = server.get("/after-write")
    assert written.status == 200
    assert (
        written.body == b"started; the request cannot be handed off once its response has started"
    )


def test_a_handoff_to_the_interface_the_application_speaks_is_refused(tmp_path: Path) -> None:
    project = tmp_path / "project"
    project.mkdir()
    (project / "app.py").write_text(
        "async def application(scope, receive, send):\n    pass\n"
        "def plain(environ, start_response):\n    return []\n"
    )
    (project / "muru.toml").write_text(
        'entrypoint = "app:application"\nhandoff = "asgi"\n\n'
        '[mounts.plain]\npath = "/plain"\nentrypoint = "app:plain"\n'
    )

    with pytest.raises(RuntimeError) as failed:
        support.start(str(project), workers=None, threads=None, chdir=None)

    assert "the release is ASGI, so handoff = 'asgi' would hand its requests to itself" in str(
        failed.value
    )

    # Declared, it is refused before anything starts.

    (project / "muru.toml").write_text(
        'entrypoint = "app:plain"\n\n'
        '[mounts.api]\npath = "/api"\nentrypoint = "app:application"\n'
        'interface = "asgi"\nhandoff = "asgi"\n'
    )

    with pytest.raises(RuntimeError) as refused:
        support.start(str(project), workers=None, threads=None, chdir=None)

    assert "mount api: handoff: an application declared interface = 'asgi'" in str(refused.value)


@pytest.fixture
def daemon(tmp_path: Path) -> Iterator[Daemon]:
    instance = start_daemon(tmp_path)

    try:
        yield instance
    finally:
        instance.stop()


def test_handoff_under_the_daemon(daemon: Daemon, tmp_path: Path) -> None:
    assert daemon.deploy("handoff", HANDOFF) == "muru: release handoff@r1 is ready and serving"

    with connect(f"ws://127.0.0.1:{daemon.port}/ws", open_timeout=10) as socket_:
        assert socket_.recv(timeout=10) == "hello from wsgi"

    assert daemon.request("/plain/x").status == 409

    # Section 7.3: the wait for a thread of each request handed from
    # ASGI to WSGI is measured in the worker and exported once its host
    # has reported, every few seconds.

    assert [daemon.request("/api/slow").status for _ in range(2)] == [200, 200]
    counted = re.compile(r"^muru_handoff_queue_wait_seconds_count\{[^}]*\} (\d+)$", re.MULTILINE)

    def handed() -> int:
        text = daemon.muructl("metrics").stdout
        return sum(int(count) for count in counted.findall(text))

    deadline = time.monotonic() + 30

    while handed() < 2:
        assert time.monotonic() < deadline, daemon.muructl("metrics").stdout
        time.sleep(0.5)

    assert "# TYPE muru_handoff_queue_wait_seconds histogram\n" in daemon.muructl("metrics").stdout

    # The setting is part of the declared release, compared on apply.

    config = tmp_path / "echo.toml"
    config.write_text(
        f"""[app.echo.release.r1]
source = '{APPS}'
entrypoint = 'plain_wsgi:application'
python = '{sys.executable}'
handoff = 'asgi'

[app.echo.release.r1.mounts.api]
path = "/api"
entrypoint = "plain_asgi:application"
interface = "asgi"
handoff = "wsgi"
"""
    )
    applied = daemon.muructl("apply", "-f", str(config))
    assert "ready and serving" in applied.stdout, applied.stdout
    assert "no changes" in daemon.muructl("apply", "-f", str(config)).stdout

    config.write_text(config.read_text().replace('handoff = "wsgi"\n', ""))
    refused = daemon.muructl("apply", "-f", str(config), check=False)
    assert refused.returncode == 1
    assert "(handoff wsgi)" in refused.stdout, refused.stdout


def test_a_registered_application_has_its_lifespan(server: support.Server) -> None:
    response = server.get("/registered")
    assert response.status == 200, response.body
    details = json.loads(response.body)

    # Its startup ran in this worker, and the request id is the request's
    # on both sides of the handoff.

    assert details["state"]["started_in"] == details["pid"]
    assert details["state"]["from_wsgi"] == details["request_id"]
    assert details["request_id"] == response.header("x-request-id")


def test_wsgi_code_reaches_the_loop(server: support.Server) -> None:
    response = server.get("/loop")
    assert (response.status, response.body) == (200, b"loop 42\n")


def test_registering_without_a_loop_fails_the_worker(tmp_path: Path) -> None:
    project = tmp_path / "project"
    project.mkdir()
    (project / "app.py").write_text(
        "def application(environ, start_response):\n    return []\n"
        "async def other(scope, receive, send):\n    pass\n"
    )
    (project / "hooks.py").write_text(
        "import muru.asgi\nfrom app import other\n\n"
        "def post_fork(context):\n    muru.asgi.register(other)\n"
    )
    (project / "muru.toml").write_text('entrypoint = "app:application"\nhooks = "hooks"\n')

    with pytest.raises(RuntimeError) as failed:
        support.start(str(project), workers=None, threads=None, chdir=None)

    assert "muru.asgi.register() was given app:other, but this worker has no event loop" in str(
        failed.value
    )


def test_a_registered_application_shuts_down_with_its_worker() -> None:
    instance = support.start(str(HANDOFF), workers=None, threads=None, chdir=None)

    try:
        assert instance.get("/registered").status == 200
    finally:
        instance.stop()

    assert "with_lifespan: shutdown" in instance.log()


def test_the_websocket_adapter_runs_callbacks_on_threads(server: support.Server) -> None:
    url = f"ws://127.0.0.1:{server.port}/chat"
    server.mark_log()

    with (
        connect(url, open_timeout=10, subprotocols=["chat"]) as first,  # type: ignore[list-item]
        connect(url, open_timeout=10, subprotocols=["chat"]) as second,  # type: ignore[list-item]
    ):
        assert first.subprotocol == "chat"
        welcome = str(first.recv(timeout=10))
        assert welcome.startswith("welcome user") and welcome.endswith(" on chat")
        name = welcome.split()[1]
        second.recv(timeout=10)

        # A value set in on_connect is there in later calls, and a send
        # from one socket's callback reaches the others.

        first.send("hello")
        assert first.recv(timeout=10) == f"{name}: hello"
        assert second.recv(timeout=10) == f"{name}: hello"

        # The callbacks run on the loop's pool for synchronous code.

        first.send("thread")
        assert str(first.recv(timeout=10)).startswith("muru-sync")

    assert server.wait_for_log(f"chat: {name} disconnected with 1000")


def test_websocket_callbacks_leave_the_application_threads_to_wsgi(server: support.Server) -> None:
    url = f"ws://127.0.0.1:{server.port}/chat"

    # As many sockets as the workers have application threads, each in a
    # callback that blocks: a WSGI request still finds a thread.

    count = 2 * len(server.worker_pids())

    with contextlib.ExitStack() as stack:
        sockets = [stack.enter_context(connect(url, open_timeout=10)) for _ in range(count)]

        for socket_ in sockets:
            socket_.recv(timeout=10)

        for socket_ in sockets:
            socket_.send("sleep")

        time.sleep(0.2)
        started = time.monotonic()
        response = server.get("/")
        assert (response.status, response.body) == (200, b"wsgi False\n")
        assert time.monotonic() - started < 1.0

        for socket_ in sockets:
            assert socket_.recv(timeout=10) == "slept"


def test_the_websocket_adapter_closes_on_an_exception(server: support.Server) -> None:
    server.mark_log()

    with connect(f"ws://127.0.0.1:{server.port}/chat", open_timeout=10) as socket_:
        socket_.recv(timeout=10)
        socket_.send("boom")

        with pytest.raises(ConnectionClosed) as closed:
            socket_.recv(timeout=10)

    assert closed.value.rcvd is not None and closed.value.rcvd.code == 1011
    assert server.wait_for_log("ValueError: boom")
    assert server.wait_for_log("disconnected with 1011 internal error")

    with connect(f"ws://127.0.0.1:{server.port}/chat", open_timeout=10) as socket_:
        socket_.recv(timeout=10)
        socket_.send("bye")

        with pytest.raises(ConnectionClosed) as closed:
            socket_.recv(timeout=10)

    assert closed.value.rcvd is not None and closed.value.rcvd.code == 4001


def test_an_asgi_request_continues_as_wsgi(server: support.Server) -> None:
    server.mark_log()

    # A body large enough to arrive in pieces: the coroutine reads the
    # first, and the WSGI callable the rest.

    sent = "".join(f"{index:07d}\n" for index in range(100_000))
    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=10)

    try:
        connection.request("POST", "/api/legacy", body=sent.encode())
        response = connection.getresponse()
        body = response.read()
        assert response.status == 200, body
        details = json.loads(body)
    finally:
        connection.close()

    # The WSGI callable gets the body the coroutine left, its state, its
    # context and the request's id, on one of the mount's own threads.

    assert 0 < len(details["state"]["first"]) < len(sent)
    assert details["state"]["first"] + details["rest"] == sent

    # A body the coroutine read to its end leaves wsgi.input empty.

    whole = server.post("/api/legacy", b"hello")
    assert json.loads(whole.body)["rest"] == ""
    assert (details["script_name"], details["path_info"]) == ("/api", "/legacy")
    assert details["context"] == "set in asgi"
    assert details["request_id"] == response.getheader("x-request-id")
    assert details["thread"].startswith("muru-app-api")
    assert server.wait_for_log(
        "after handoff: receive() called after the request was handed to WSGI"
    )
    assert server.wait_for_log("handoff=wsgi")


def test_handoffs_to_wsgi_wait_for_a_thread_in_turn(server: support.Server) -> None:
    # The mount has one thread of its own, so two slow handoffs take turns.

    started = time.monotonic()
    results: list[int] = []

    def fetch() -> None:
        results.append(server.get("/api/slow").status)

    threads = [threading.Thread(target=fetch) for _ in range(2)]

    for thread in threads:
        thread.start()

    for thread in threads:
        thread.join()

    assert results == [200, 200]
    assert time.monotonic() - started >= 1.0


def test_an_asgi_mount_without_the_setting_raises(server: support.Server) -> None:
    response = server.get("/async-plain/x")
    assert response.status == 409
    assert response.body == (
        b"HandoffNotEnabled: mount asyncplain does not enable handing requests to WSGI; "
        b'set handoff = "wsgi" on the mount\n'
    )

    started = server.get("/api/started")
    assert started.status == 200
    assert started.body == b"the request cannot be handed off once its response has started"


ASGI_RELEASE = """
import threading

from muru.asgi import websocket


def legacy(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    body = environ["wsgi.input"].read()
    return [f"wsgi {threading.current_thread().name} {body.decode()}".encode()]


class Echo:
    def on_receive(self, ws, message):
        ws.send(threading.current_thread().name)


echo = websocket.Handler(Echo())


async def application(scope, receive, send):
    if scope["type"] == "websocket":
        return await echo(scope, receive, send)

    if scope["type"] != "http":
        return

    return scope["extensions"]["muru"]["asgi_to_wsgi"].handoff(legacy)
"""


def test_an_asgi_release_hands_requests_to_its_threads(tmp_path: Path) -> None:
    project = tmp_path / "project"
    project.mkdir()
    (project / "app.py").write_text(ASGI_RELEASE)
    (project / "muru.toml").write_text(
        'entrypoint = "app:application"\nhandoff = "wsgi"\nthreads = 2\n'
    )
    instance = support.start(str(project), workers=None, threads=None, chdir=None)

    try:
        response = instance.post("/anything", b"payload")
        assert response.status == 200, response.body
        assert response.body.startswith(b"wsgi muru-app-")
        assert response.body.endswith(b" payload")

        # The WebSocket adapter in an ASGI worker runs on the loop's pool
        # for synchronous code.

        with connect(f"ws://127.0.0.1:{instance.port}/ws", open_timeout=10) as socket_:
            socket_.send("which")
            assert str(socket_.recv(timeout=10)).startswith("muru-sync")
    finally:
        instance.stop()

    assert "with 1 worker" in instance.log()
