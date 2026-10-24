"""WebSockets through the edge to an ASGI application (design specification section 5.6).

The edge completes the handshake once the application accepts, relays
whole messages in both directions, carries close codes each way, enforces
the message size limit, and turns a decline into the response the
application chose.
"""

from __future__ import annotations

import json
import socket
from collections.abc import Iterator

import pytest
from support import server as support
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
