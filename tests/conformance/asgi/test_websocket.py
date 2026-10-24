"""The websocket scope and its messages (section 5.6 of the design specification)."""

from __future__ import annotations

import json

import pytest
from support.server import Server
from websockets.exceptions import ConnectionClosed, InvalidStatus
from websockets.sync.client import ClientConnection, connect


def open_socket(server: Server, path: str, **options: object) -> ClientConnection:
    return connect(f"ws://127.0.0.1:{server.port}{path}", open_timeout=10, **options)  # type: ignore[arg-type]


def test_connect_is_the_first_message(server: Server) -> None:
    server.mark_log()
    with open_socket(server, "/ws/scope") as socket_:
        report = json.loads(socket_.recv())

    assert server.wait_for_log("first websocket message websocket.connect")
    assert report["type"] == "websocket"
    assert report["scheme"] == "ws"
    assert report["path"] == "/ws/scope"
    assert report["subprotocols"] == []
    assert "method" not in report


def test_message_types_and_close_sequence(server: Server) -> None:
    server.mark_log()

    with open_socket(server, "/ws/types") as socket_:
        socket_.send("one")
        socket_.send(b"two")
        socket_.send("done")
        kinds = json.loads(socket_.recv())
        with pytest.raises(ConnectionClosed) as closed:
            socket_.recv()

    assert kinds == ["text", "bytes", "text"]
    assert closed.value.rcvd is not None
    assert closed.value.rcvd.code == 1000
    assert server.wait_for_log("after close websocket.disconnect")


def test_close_code_and_reason_from_the_application(server: Server) -> None:
    with open_socket(server, "/ws/close-reason") as socket_:
        with pytest.raises(ConnectionClosed) as closed:
            socket_.recv()

    assert closed.value.rcvd is not None
    assert closed.value.rcvd.code == 4042
    assert closed.value.rcvd.reason == "because"


def test_client_close_code_is_reported(server: Server) -> None:
    server.mark_log()
    with open_socket(server, "/ws/types") as socket_:
        socket_.close(code=4001, reason="bye")

    assert server.wait_for_log("disconnect:4001:bye") or server.wait_for_log(
        "first websocket message"
    )


def test_send_before_accept_is_a_500(server: Server) -> None:
    server.mark_log()
    with pytest.raises(InvalidStatus) as refused:
        open_socket(server, "/ws/send-before-accept")

    assert refused.value.response.status_code == 500
    assert server.wait_for_log("websocket.send sent before websocket.accept")


@pytest.mark.parametrize(
    ("path", "message"),
    [
        ("/ws/accept-twice", "websocket.accept sent after the WebSocket was accepted"),
        ("/ws/both", "needs exactly one of bytes and text"),
    ],
)
def test_invalid_send_after_accept_closes_with_1011(
    server: Server, path: str, message: str
) -> None:
    server.mark_log()

    with open_socket(server, path) as socket_:
        with pytest.raises(ConnectionClosed) as closed:
            socket_.recv()

    assert closed.value.rcvd is not None
    assert closed.value.rcvd.code == 1011
    assert server.wait_for_log(message)
