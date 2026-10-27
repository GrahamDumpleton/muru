"""WebSockets through the edge to an ASGI application (design specification section 5.6).

The edge completes the handshake once the application accepts, relays
whole messages in both directions, carries close codes each way, enforces
the message size limit, and turns a decline into the response the
application chose.
"""

from __future__ import annotations

import json
import socket
import sys
import time
import urllib.request
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import start_daemon
from websockets.exceptions import ConnectionClosed, InvalidStatus
from websockets.sync.client import ClientConnection, connect


@pytest.fixture(scope="module")
def server() -> Iterator[support.Server]:
    instance = support.start("plain_asgi:application", workers=1, threads=None)

    try:
        yield instance
    finally:
        instance.stop()


def open_socket(server: support.Server, path: str, **options: object) -> ClientConnection:
    """Open a WebSocket to the fixture at `path`."""

    return connect(f"ws://127.0.0.1:{server.port}{path}", open_timeout=10, **options)  # type: ignore[arg-type]


def test_echo_text_and_binary(server: support.Server) -> None:
    with open_socket(server, "/ws/echo") as socket_:
        socket_.send("hello")
        assert socket_.recv() == "hello"
        socket_.send(b"\x00\x01\x02")
        assert socket_.recv() == b"\x00\x01\x02"

    assert "101 GET /ws/echo" in server.log()


def test_scope_and_accept_headers(server: support.Server) -> None:
    with open_socket(server, "/ws/scope", subprotocols=["chat", "other"]) as socket_:
        assert socket_.subprotocol == "chat"
        assert socket_.response is not None
        assert socket_.response.headers["x-ws"] == "yes"
        report = json.loads(socket_.recv())

    assert report["type"] == "websocket"
    assert report["scheme"] == "ws"
    assert report["path"] == "/ws/scope"
    assert report["subprotocols"] == ["chat", "other"]
    assert "websocket.http.response" in report["extensions"]
    assert "muru" in report["extensions"]
    assert report["state"]["started_in"]
    assert "method" not in report


def test_client_close_code_reaches_the_application(server: support.Server) -> None:
    server.mark_log()

    with open_socket(server, "/ws/echo") as socket_:
        socket_.send("one")
        assert socket_.recv() == "one"
        socket_.close(code=4000, reason="done")

    assert server.wait_for_log("websocket disconnected with 4000")


def test_application_close_code_reaches_the_client(server: support.Server) -> None:
    with open_socket(server, "/ws/close-code") as socket_:
        with pytest.raises(ConnectionClosed) as closed:
            socket_.recv()

    assert closed.value.rcvd is not None
    assert closed.value.rcvd.code == 4001
    assert closed.value.rcvd.reason == "custom"


def test_close_before_accept_is_403(server: support.Server) -> None:
    with pytest.raises(InvalidStatus) as refused:
        open_socket(server, "/ws/decline")

    assert refused.value.response.status_code == 403


def test_returning_without_accepting_is_403(server: support.Server) -> None:
    with pytest.raises(InvalidStatus) as refused:
        open_socket(server, "/ws/return")

    assert refused.value.response.status_code == 403


def test_decline_with_a_full_response(server: support.Server) -> None:
    with pytest.raises(InvalidStatus) as refused:
        open_socket(server, "/ws/http-decline")

    assert refused.value.response.status_code == 401
    assert refused.value.response.body == b"no socket for you\n"


def test_exception_after_accept_closes_with_1011(server: support.Server) -> None:
    server.mark_log()

    with open_socket(server, "/ws/error") as socket_:
        with pytest.raises(ConnectionClosed) as closed:
            socket_.recv()

    assert closed.value.rcvd is not None
    assert closed.value.rcvd.code == 1011
    assert server.wait_for_log("deliberate failure on the socket")


def test_send_after_close_is_an_error_in_the_log(server: support.Server) -> None:
    server.mark_log()

    with open_socket(server, "/ws/send-after-close") as socket_:
        with pytest.raises(ConnectionClosed):
            socket_.recv()

    assert server.wait_for_log("websocket.send sent after the WebSocket was closed")


def test_message_over_the_limit_closes_with_1009(server: support.Server) -> None:
    with open_socket(server, "/ws/echo", max_size=None) as socket_:
        # The edge may close while the message is still being sent.

        with pytest.raises(ConnectionClosed) as closed:
            socket_.send("x" * (1024 * 1024 + 1))
            socket_.recv()

    assert closed.value.rcvd is not None
    assert closed.value.rcvd.code == 1009


def test_client_vanishing_is_a_1006_disconnect(server: support.Server) -> None:
    server.mark_log()

    with socket.create_connection(("127.0.0.1", server.port), timeout=5) as raw:
        raw.sendall(
            b"GET /ws/echo HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n"
            b"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        )
        assert raw.recv(65536).startswith(b"HTTP/1.1 101")

        # Gone without a close frame.

    assert server.wait_for_log("websocket disconnected with 1006")


def test_http_requests_still_work_on_the_same_worker(server: support.Server) -> None:
    with open_socket(server, "/ws/echo") as socket_:
        response = server.get("/")
        assert response.status == 200
        socket_.send("after http")
        assert socket_.recv() == "after http"


def test_upgrade_to_a_wsgi_application_is_an_ordinary_request() -> None:
    instance = support.start("plain_wsgi:application", workers=1)

    try:
        with socket.create_connection(("127.0.0.1", instance.port), timeout=5) as raw:
            raw.sendall(
                b"GET / HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n"
                b"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            answer = raw.recv(65536)
    finally:
        instance.stop()

    assert answer.startswith(b"HTTP/1.1 200")
    assert b"Hello, world!" in answer


def metric(text: str, name: str, *labels: str) -> float:
    """Sum the samples of `name` whose labels include each of `labels`."""

    total = 0.0

    for line in text.splitlines():
        if (
            line.startswith(name)
            and line[len(name)] in "{ "
            and all(label in line for label in labels)
        ):
            total += float(line.rsplit(" ", 1)[1])

    return total


def wait_for(predicate: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.05)


def test_open_websockets_are_counted_apart_from_requests() -> None:
    port = support.free_port()
    instance = support.start(
        "plain_asgi:application", workers=1, threads=None, args=["--dashboard", f"127.0.0.1:{port}"]
    )
    dashboard = f"http://127.0.0.1:{port}"

    def read(path: str) -> str:
        with urllib.request.urlopen(dashboard + path, timeout=10) as response:
            body: bytes = response.read()
            return body.decode()

    try:
        assert instance.wait_for_log(f"dashboard on {dashboard}/")

        with (
            open_socket(instance, "/ws/echo") as first,
            open_socket(instance, "/ws/echo") as second,
        ):
            first.send("hello")
            assert first.recv() == "hello"
            second.send("there")
            assert second.recv() == "there"

            # The handshake is a request that ended when it was accepted:
            # its line is written then, and it no longer counts in flight.

            assert instance.wait_for_log("101 GET /ws/echo")
            metrics = read("/metrics")
            assert metric(metrics, "muru_websocket_connections", 'mount="default"') == 2
            assert metric(metrics, "muru_requests_in_flight") == 0
            assert metric(metrics, "muru_requests_total", 'status="1xx"') == 2

            view = json.loads(read("/v1/websockets"))
            assert view["open"] == 2
            assert [(row["pattern"], row["open"]) for row in view["endpoints"]] == [("/ws/echo", 2)]
            assert {socket_["path"] for socket_ in view["sockets"]} == {"/ws/echo"}
            assert view["sockets"][0]["received"] == 1 and view["sockets"][0]["sent"] == 1
            assert "WebSockets open" in read("/fragment/overview")

        # Each close is a line of its own, with who closed, the code and
        # the messages each way; the lifetime goes to its own histogram.

        assert instance.wait_for_log("websocket=closed by=client code=1000 received=1 sent=1")
        wait_for(lambda: metric(read("/metrics"), "muru_websocket_connections") == 0)
        metrics = read("/metrics")
        assert metric(metrics, "muru_websockets_closed_total", 'by="client"', 'close="normal"') == 2
        assert metric(metrics, "muru_websocket_duration_seconds_count") == 2
        assert json.loads(read("/v1/websockets"))["open"] == 0
    finally:
        instance.stop()


def test_the_daemon_counts_and_lists_open_websockets(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)
    config = tmp_path / "chat.toml"
    config.write_text(
        f"""[app.chat.release.r1]
source = '{support.FIXTURES}'
entrypoint = 'plain_asgi:application'
interface = 'asgi'
python = '{sys.executable}'
"""
    )

    def words(*command: str) -> str:
        return " ".join(daemon.muructl(*command).stdout.split())

    try:
        assert "ready and serving" in daemon.muructl("apply", "-f", str(config)).stdout
        assert words("websockets") == "no WebSockets open"

        with connect(f"ws://127.0.0.1:{daemon.port}/ws/echo/42", open_timeout=10) as socket_:
            socket_.send("hello")
            assert socket_.recv() == "hello"

            assert "APP ENDPOINT OPEN chat /ws/echo/{id} 1" in words("websockets")
            listed = words("websockets", "chat", "--list")
            assert "chat@r1 /ws/echo/42" in listed and " 1 1 0 " in listed

            # The counts in status and release show are sampled each second.

            wait_for(lambda: "websockets 1 open" in words("status"))
            assert "websockets 1 open" in words("release", "show", "chat@r1")

        wait_for(lambda: words("websockets") == "no WebSockets open")
        access = daemon.muructl("logs", "--access").stdout
        assert "101 GET /ws/echo/42" in access
        assert "websocket=closed by=client code=1000 received=1 sent=1" in access
    finally:
        daemon.stop()
