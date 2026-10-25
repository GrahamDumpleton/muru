"""Timeouts enforced by the edge (design specification sections 4.6 and 7.6)."""

from __future__ import annotations

import json
import socket
import time
from concurrent.futures import ThreadPoolExecutor

from support import server as support


def test_queue_timeout_answers_503_queue_full() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=1, args=["--timeout", "queue=1s"]
    )

    try:
        with ThreadPoolExecutor(max_workers=2) as pool:
            slow = pool.submit(server.get, "/sleep?seconds=3")

            # Long enough for the slow request to hold the only slot, even
            # on a busy machine.

            time.sleep(1.0)
            started = time.monotonic()
            queued = server.get("/", headers={"Accept": "application/json"})
            waited = time.monotonic() - started
            assert slow.result().body == b"slept"

        assert queued.status == 503
        assert json.loads(queued.body)["reason"] == "queue_full"
        assert queued.header("Retry-After") == "1"
        assert 0.8 < waited < 2.5
        assert server.get("/").status == 200
    finally:
        server.stop()


def test_request_deadline_answers_504_and_frees_the_edge() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=2, args=["--timeout", "deadline=1s"]
    )

    try:
        started = time.monotonic()
        response = server.get("/sleep?seconds=3", headers={"Accept": "application/json"})
        assert time.monotonic() - started < 2.5
        assert response.status == 504
        assert json.loads(response.body)["reason"] == "deadline"
        assert server.get("/").body == b"Hello, world!\n"
    finally:
        server.stop()


def test_body_idle_timeout_answers_408() -> None:
    server = support.start("plain_wsgi:application", workers=1, args=["--timeout", "body_idle=1s"])

    try:
        with socket.create_connection(("127.0.0.1", server.port), timeout=10) as sock:
            sock.sendall(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 20\r\n\r\npartial")
            data = b""

            while True:
                chunk = sock.recv(65536)
                if not chunk:
                    break
                data += chunk

        assert data.startswith(b"HTTP/1.1 408"), data
        assert b"reason: client_timeout" in data
        assert server.get("/").status == 200
    finally:
        server.stop()


def test_header_read_timeout_closes_the_connection() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, args=["--timeout", "header_read=1s"]
    )

    try:
        with socket.create_connection(("127.0.0.1", server.port), timeout=10) as sock:
            sock.sendall(b"GET / HTTP/1.1\r\nHost: t\r\n")
            started = time.monotonic()
            data = sock.recv(65536)
            assert time.monotonic() - started < 5

        # hyper closes the connection, with or without a 408 first.

        assert data == b"" or data.startswith(b"HTTP/1.1 408")
        assert server.get("/").status == 200
    finally:
        server.stop()
