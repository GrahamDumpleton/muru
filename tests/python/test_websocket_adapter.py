"""The WebSocket adapter outside Muru, driven as any ASGI server would drive it."""

from __future__ import annotations

import asyncio
import contextvars
import threading
from typing import Any

from muru.asgi import websocket

name: contextvars.ContextVar[str] = contextvars.ContextVar("name", default="")


class Recorder:
    def __init__(self) -> None:
        self.calls: list[tuple[str, Any, str]] = []

    def on_connect(self, ws: websocket.Socket) -> None:
        name.set("alice")
        self.calls.append(("connect", ws.subprotocol, threading.current_thread().name))
        ws.send("hi")

    def on_receive(self, ws: websocket.Socket, message: str | bytes) -> None:
        if message == b"boom":
            raise ValueError("boom")

        self.calls.append(("receive", message, name.get()))
        ws.send(message)

    def on_disconnect(self, ws: websocket.Socket, code: int, reason: str) -> None:
        self.calls.append(("disconnect", code, reason))


def drive(
    app: Any, incoming: list[dict[str, Any]], subprotocols: list[str]
) -> list[dict[str, Any]]:
    """Run the application against a scripted client; return what it sent."""

    sent: list[dict[str, Any]] = []

    async def main() -> None:
        queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue()

        for message in incoming:
            queue.put_nowait(message)

        async def receive() -> dict[str, Any]:
            return await queue.get()

        async def send(message: dict[str, Any]) -> None:
            sent.append(message)

        scope = {"type": "websocket", "subprotocols": subprotocols}
        await asyncio.wait_for(app(scope, receive, send), 10)

    asyncio.run(main())
    return sent


def test_callbacks_run_in_order_in_one_context_on_threads_of_its_own() -> None:
    recorder = Recorder()
    app = websocket.Handler(recorder, subprotocol="chat", headers=[("x-chat", "yes")])
    sent = drive(
        app,
        [
            {"type": "websocket.connect"},
            {"type": "websocket.receive", "text": "one"},
            {"type": "websocket.receive", "bytes": b"two"},
            {"type": "websocket.disconnect", "code": 1001, "reason": "away"},
        ],
        ["chat"],
    )

    assert sent[0] == {
        "type": "websocket.accept",
        "headers": [(b"x-chat", b"yes")],
        "subprotocol": "chat",
    }
    assert sent[1:] == [
        {"type": "websocket.send", "text": "hi"},
        {"type": "websocket.send", "text": "one"},
        {"type": "websocket.send", "bytes": b"two"},
    ]
    assert recorder.calls[0][:2] == ("connect", "chat")
    assert recorder.calls[0][2].startswith("muru-websocket")
    assert recorder.calls[1:] == [
        ("receive", "one", "alice"),
        ("receive", b"two", "alice"),
        ("disconnect", 1001, "away"),
    ]


def test_an_exception_closes_with_1011_and_send_then_raises() -> None:
    sockets: list[websocket.Socket] = []

    class Keeper(Recorder):
        def on_connect(self, ws: websocket.Socket) -> None:
            sockets.append(ws)

    keeper = Keeper()
    sent = drive(
        websocket.Handler(keeper, subprotocol="chat"),
        [{"type": "websocket.connect"}, {"type": "websocket.receive", "bytes": b"boom"}],
        [],
    )

    assert sent[0] == {"type": "websocket.accept", "headers": []}
    assert sent[-1] == {"type": "websocket.close", "code": 1011, "reason": "internal error"}
    assert sockets[0].closed

    try:
        sockets[0].send("late")
    except websocket.ConnectionClosed as error:
        assert (error.code, error.reason) == (1011, "internal error")
    else:
        raise AssertionError("send after the close did not raise")

    assert keeper.calls == [("disconnect", 1011, "internal error")]
