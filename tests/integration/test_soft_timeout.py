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
import re
import socket
import threading
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

        # Section 11.7: the event carries where the request was, taken
        # before it was interrupted.

        assert server.wait_for_log("request.soft_timeout: worker 0")
        assert re.search(
            r"request\.soft_timeout: worker 0 \(pid \d+\): request \S+ GET /busy\?seconds=10 "
            r"ran past the soft timeout of 1 s at \S+ line \d+ in \w+",
            server.log_since_mark(),
        ), server.log_since_mark()

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


def test_the_hard_timeout_fires_with_a_second_request_on_a_stuck_worker() -> None:
    """A request stuck in C code holding the GIL is replaced by the hard
    timeout even when a second request reaches the same worker. The
    worker's reports come from a thread of their own, so the second
    request's arrival, which blocks a link thread on the interpreter,
    does not stop the host seeing the first request's age."""

    server = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=2,
        args=["--timeout", "hard=3s", "--timeout", "shutdown=1s"],
    )

    def hold() -> None:
        try:
            server.get("/hold?seconds=30")
        except OSError:
            pass

    try:
        worker = pid_of(server)
        first = threading.Thread(target=hold, daemon=True)
        first.start()
        time.sleep(1)
        second = threading.Thread(target=hold, daemon=True)
        second.start()
        assert server.wait_for_log("past the hard timeout", timeout=10), server.log()
        assert "GIL stall" not in server.log()
        first.join(timeout=30)
        second.join(timeout=30)
        assert pid_of(server) != worker
    finally:
        server.stop()


def test_the_hard_timeout_answers_the_stuck_request_at_once() -> None:
    """The request that condemned the worker cannot finish, so its client
    gets 504 hard_timeout when the worker is replaced, not after the drain
    grace and the shutdown timeout that the worker's other requests get."""

    server = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=2,
        args=[
            "--timeout",
            "hard=2s",
            "--timeout",
            "hard_drain_grace=20s",
            "--timeout",
            "shutdown=20s",
        ],
    )

    try:
        started = time.monotonic()
        response = server.get("/hold?seconds=30", headers={"Accept": "application/json"})
        answered = time.monotonic() - started
        assert response.status == 504, response.body
        assert json.loads(response.body)["reason"] == "hard_timeout"
        assert answered < 8, answered

        # The worker's answer to the host, which logs it, follows the edge's
        # answer to the client by a moment.

        assert server.wait_for_log("gave up 1 request for the edge to answer")
    finally:
        server.stop()
