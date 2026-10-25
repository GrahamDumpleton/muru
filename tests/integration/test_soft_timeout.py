"""The soft timeout (design specification section 7.6).

A request that runs past the soft timeout is interrupted where it is: a
WSGI thread gets ``muru.RequestTimeout`` raised in it, even while it waits
in the bridge for the request body, and an ASGI task is cancelled. The
edge answers 504 ``soft_timeout`` unless the application answers for
itself, the worker carries on serving, and streaming responses and
WebSockets are exempt from the soft and hard timeouts.
"""

from __future__ import annotations

import json
import socket
import time

import pytest
from support import server as support
from websockets.sync.client import connect


def pid_of(server: support.Server) -> int:
    return int(json.loads(server.get("/info").body)["pid"])


def read_response(sock: socket.socket) -> bytes:
    """Read one response with a Content-Length from the socket, and no more."""

    data = b""

    while b"\r\n\r\n" not in data:
        chunk = sock.recv(65536)
        assert chunk, data
        data += chunk

    head, _, body = data.partition(b"\r\n\r\n")
    length = next(
        int(line.split(b":", 1)[1])
        for line in head.split(b"\r\n")
        if line.lower().startswith(b"content-length:")
    )

    while len(body) < length:
        chunk = sock.recv(65536)
        assert chunk, data
        body += chunk

    return head + b"\r\n\r\n" + body


def test_a_wsgi_request_past_the_soft_timeout_is_interrupted() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=2, args=["--timeout", "soft=1s"]
    )

    try:
        worker = pid_of(server)
        server.mark_log()
        started = time.monotonic()
        response = server.get("/busy?seconds=10", headers={"Accept": "application/json"})
        waited = time.monotonic() - started
        assert response.status == 504, response.body
        assert json.loads(response.body)["reason"] == "soft_timeout"
        assert waited < 3, waited

        # The log says which request was interrupted and where it was.

        assert server.wait_for_log("ran past the soft timeout of 1 s")
        log = server.log_since_mark()
        assert "GET /busy?seconds=10" in log
        assert "muru.RequestTimeout" in log and "plain_wsgi.py" in log, log

        # The worker is the same one, and nothing is left over from the
        # interruption for the requests that follow on its threads.

        assert pid_of(server) == worker

        for _ in range(6):
            assert server.get("/busy?seconds=0.05").body == b"busy"
    finally:
        server.stop()


def test_a_thread_waiting_for_the_body_is_woken() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=2, args=["--timeout", "soft=1s"]
    )

    try:
        started = time.monotonic()

        with socket.create_connection(("127.0.0.1", server.port), timeout=10) as sock:
            sock.sendall(
                b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 100\r\n"
                b"Accept: application/json\r\n\r\npartial"
            )
            data = read_response(sock)

        assert time.monotonic() - started < 3
        assert data.startswith(b"HTTP/1.1 504"), data
        assert b'"reason":"soft_timeout"' in data.replace(b" ", b""), data
        assert server.get("/").status == 200
    finally:
        server.stop()


def test_an_application_can_answer_a_timeout_itself() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=2, args=["--timeout", "soft=1s"]
    )

    try:
        response = server.get("/busy-handled?seconds=10")
        assert response.status == 503
        assert response.body == b"took too long\n"
        assert server.get("/busy-handled?seconds=0.05").body == b"busy"
    finally:
        server.stop()


def test_an_asgi_request_past_the_soft_timeout_is_cancelled() -> None:
    server = support.start(
        "plain_asgi:application", workers=1, threads=None, args=["--timeout", "soft=1s"]
    )

    try:
        server.mark_log()
        started = time.monotonic()
        response = server.get("/sleep?seconds=10", headers={"Accept": "application/json"})
        assert time.monotonic() - started < 3
        assert response.status == 504, response.body
        assert json.loads(response.body)["reason"] == "soft_timeout"
        assert server.wait_for_log("ran past the soft timeout of 1 s")
        assert "CancelledError" in server.log_since_mark()
        assert server.get("/sleep?seconds=0.05").body == b"slept\n"
    finally:
        server.stop()


@pytest.mark.parametrize("app", ["plain_wsgi:application", "plain_asgi:application"])
def test_streaming_responses_are_exempt_from_the_timeouts(app: str) -> None:
    threads = 2 if app.startswith("plain_wsgi") else None
    server = support.start(
        app,
        workers=1,
        threads=threads,
        args=["--timeout", "soft=1s", "--timeout", "hard=6s", "--timeout", "shutdown=1s"],
    )

    try:
        worker = pid_of(server)
        response = server.get("/events?count=5&delay=0.4")
        assert response.status == 200
        assert response.body.count(b"data: ") == 5
        assert "soft timeout" not in server.log()
        assert pid_of(server) == worker
    finally:
        server.stop()


def test_websockets_are_exempt_from_the_hard_timeout() -> None:
    server = support.start(
        "plain_asgi:application",
        workers=1,
        threads=None,
        args=["--timeout", "hard=1s", "--timeout", "shutdown=1s"],
    )

    try:
        worker = pid_of(server)

        with connect(f"ws://127.0.0.1:{server.port}/ws/echo", open_timeout=10) as socket_:
            time.sleep(2.5)
            socket_.send("still here")
            assert socket_.recv() == "still here"

        assert "worker.timeout_killed" not in server.log()
        assert pid_of(server) == worker
    finally:
        server.stop()


def test_the_hard_timeout_must_exceed_the_soft_timeout() -> None:
    with pytest.raises(RuntimeError, match="hard_timeout must exceed soft_timeout by at least 5s"):
        support.start(
            "plain_wsgi:application",
            workers=1,
            args=["--timeout", "soft=10s", "--timeout", "hard=12s"],
        )
