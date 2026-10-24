"""Event webhooks (design specification section 11.5).

Each [[events.sink]] of the daemon's configuration file receives the
events it is filtered to as signed CloudEvents requests, in order, from a
cursor the store keeps: a receiver that is down gets everything once it
is back, and so does the sink after a daemon restart.
"""

from __future__ import annotations

import hashlib
import hmac
import http.server
import json
import threading
import time
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest
from support import server as support
from support.daemon import SHOP, start_daemon

SECRET = b"hook-secret"


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.05)


class Receiver:
    """An HTTP server that records deliveries and can refuse them."""

    def __init__(self, port: int) -> None:
        self.port = port
        self.deliveries: list[tuple[dict[str, str], bytes]] = []
        self.refuse = False
        self.lock = threading.Lock()
        receiver = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))

                with receiver.lock:
                    refuse = receiver.refuse

                    if not refuse:
                        receiver.deliveries.append(
                            ({name.lower(): value for name, value in self.headers.items()}, body)
                        )

                self.send_response(503 if refuse else 204)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, format: str, *args: Any) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def start(self) -> None:
        self.thread.start()

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def events(self) -> list[dict[str, Any]]:
        with self.lock:
            return [json.loads(body) for _, body in self.deliveries]

    def types(self) -> list[str]:
        return [event["type"] for event in self.events()]


def sink_config(port: int, **extra: str) -> str:
    lines = [
        "[[events.sink]]",
        "name = 'ops'",
        f"url = 'http://127.0.0.1:{port}/hook'",
        f"secret = '{SECRET.decode()}'",
    ]
    lines.extend(f"{key} = {value}" for key, value in extra.items())
    return "\n".join(lines) + "\n"


@pytest.fixture
def receiver() -> Iterator[Receiver]:
    instance = Receiver(support.free_port())
    instance.start()

    try:
        yield instance
    finally:
        instance.stop()


def test_events_arrive_signed_in_order_and_filtered(tmp_path: Path, receiver: Receiver) -> None:
    daemon = start_daemon(
        tmp_path,
        listeners=[(support.free_port(), False)],
        config_extra=sink_config(receiver.port, types="['release.*', 'traffic.*']"),
    )

    try:
        assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
        wait_for(lambda: "dev.muru.traffic.switched" in receiver.types())
        events = receiver.events()
        assert [event["type"] for event in events] == [
            "dev.muru.release.created",
            "dev.muru.release.ready",
            "dev.muru.traffic.switched",
        ]

        # CloudEvents attributes, and the sequence numbers rise.

        ready = events[1]
        assert ready["specversion"] == "1.0"
        assert ready["subject"] == "shop@r1"
        assert ready["source"].startswith("muru://")
        assert ready["time"].endswith("Z")
        assert ready["datacontenttype"] == "application/json"
        assert ready["level"] == "info"
        assert "ready" in ready["data"]["message"]
        assert ready["id"] == str(ready["muruseq"])
        assert [event["muruseq"] for event in events] == sorted(
            event["muruseq"] for event in events
        )

        # Each delivery is signed with the secret over its exact body.

        for headers, body in receiver.deliveries:
            expected = "sha256=" + hmac.new(SECRET, body, hashlib.sha256).hexdigest()
            assert headers["x-muru-signature"] == expected
            assert headers["content-type"] == "application/cloudevents+json"
            assert headers["x-muru-event"] == json.loads(body)["type"]
            assert headers["x-muru-delivery"] == json.loads(body)["id"]
            assert headers["user-agent"].startswith("muru/")
    finally:
        daemon.stop()


def test_a_receiver_outage_delays_nothing_but_delivery(tmp_path: Path, receiver: Receiver) -> None:
    daemon = start_daemon(
        tmp_path,
        listeners=[(support.free_port(), False)],
        config_extra=sink_config(receiver.port, types="['release.*', 'events.*']"),
    )

    try:
        receiver.refuse = True
        assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"

        # The daemon reports the outage once, after a few attempts, and
        # keeps retrying; nothing has been accepted yet.

        wait_for(lambda: "events.sink.failing" in daemon.events())
        assert receiver.events() == []
        receiver.refuse = False
        wait_for(lambda: "dev.muru.events.sink.recovered" in receiver.types())
        types = receiver.types()
        assert types[:2] == ["dev.muru.release.created", "dev.muru.release.ready"]
        assert "dev.muru.events.sink.failing" in types
        assert "events.sink.recovered" in daemon.events()
        assert len(types) == len(set(json.loads(body)["id"] for _, body in receiver.deliveries))
    finally:
        daemon.stop()


def test_the_cursor_survives_a_daemon_restart(tmp_path: Path, receiver: Receiver) -> None:
    listeners = [(support.free_port(), False)]
    config = sink_config(receiver.port, types="['release.*']")
    first = start_daemon(tmp_path, listeners=listeners, config_extra=config)
    second = None

    try:
        assert first.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
        wait_for(lambda: "dev.muru.release.ready" in receiver.types())
        before = receiver.events()

        # The next deploy happens while the receiver is down; the daemon
        # detaches with those deliveries pending.

        receiver.refuse = True
        assert first.deploy("shop", SHOP) == "muru: release shop@r2 is ready and serving"
        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        receiver.refuse = False

        # The next daemon carries on from the cursor: r2's events arrive
        # once, and nothing already delivered is sent again.

        second = start_daemon(
            tmp_path, run_dir=first.run_dir, listeners=listeners, config_extra=config
        )
        wait_for(
            lambda: any(
                event["subject"] == "shop@r2" and event["type"] == "dev.muru.release.ready"
                for event in receiver.events()
            )
        )
        wait_for(
            lambda: any(event["type"] == "dev.muru.release.adopted" for event in receiver.events())
        )
        events = receiver.events()
        ids = [event["id"] for event in events]
        assert len(ids) == len(set(ids)), "an event was delivered twice"
        assert events[: len(before)] == before
    finally:
        if second is None:
            second = start_daemon(
                tmp_path, run_dir=first.run_dir, listeners=listeners, config_extra=config
            )

        second.stop(shutdown=True)
