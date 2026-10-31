"""The steps of the ASGI and WebSockets tutorial, with the tutorial's own code.

docs/tutorials/asgi-websockets.md includes docs/tutorials/code/chat/chat.py,
and this test runs that file through the page's steps, so a change that
breaks one fails here rather than for a reader.
"""

from __future__ import annotations

import json
import urllib.request

from support import server as support
from websockets.exceptions import ConnectionClosed
from websockets.sync.client import connect

CHAT = support.ROOT / "docs" / "tutorials" / "code" / "chat"


def page(server: support.Server) -> str:
    """What the chat's page says."""

    return server.get("/").body.decode()


def test_the_chat_room_tutorial() -> None:
    dashboard = support.free_port()
    server = support.start(
        "chat:application",
        workers=None,
        threads=None,
        chdir=CHAT,
        args=["--dashboard", f"127.0.0.1:{dashboard}"],
    )

    try:
        # The port accepts connections before muructl has connected to
        # the worker and written its serving line, so that is waited for.

        assert server.wait_for_log("chat:application is served as ASGI")
        assert server.wait_for_log("with 1 worker of concurrency 1000")
        assert page(server) == "0 in the chat\n"

        # Join the chat: what one member says the other hears, and the
        # page counts both.

        url = f"ws://127.0.0.1:{server.port}/chat"

        with connect(url, open_timeout=10) as first, connect(url, open_timeout=10) as second:
            assert page(server) == "2 in the chat\n"
            first.send("hello from the first terminal")
            assert second.recv(timeout=5) == "hello from the first terminal"
            assert server.wait_for_log("101 GET /chat")

            # Watch the sockets: the dashboard lists both under /chat.

            with urllib.request.urlopen(f"http://127.0.0.1:{dashboard}/v1/websockets") as answer:
                listing = json.load(answer)

            assert listing["open"] == 2
            assert [endpoint["pattern"] for endpoint in listing["endpoints"]] == ["/chat"]

        # Leave the chat: a client's close is logged with what it sent.

        assert server.wait_for_log("websocket=closed by=client code=1000 received=1 sent=0")
        assert server.wait_for_log("websocket=closed by=client code=1000 received=0 sent=1")
        assert page(server) == "0 in the chat\n"

        # Stop with sockets open: each member, once it has spoken, is told
        # the server is going away.

        with connect(url, open_timeout=10) as first, connect(url, open_timeout=10) as second:
            for member in (first, second):
                member.send("still here")

            server.stop()

            for member in (first, second):
                try:
                    while True:
                        member.recv(timeout=10)
                except ConnectionClosed as closed:
                    assert closed.rcvd is not None
                    assert closed.rcvd.code == 1001

        assert "websocket=closed by=edge code=1001" in server.log()
    finally:
        server.stop()
