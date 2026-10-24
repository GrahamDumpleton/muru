"""Integration tests: muructl run with a preloaded, forked WSGI application.

These need the muructl binary (target/debug/muructl, or MURUCTL) and the
extension built into the test interpreter (`just dev`). They skip otherwise.
"""

from __future__ import annotations

import http.client
import json
import os
import socket
import sys
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor

import pytest
from support import server as support

WORKERS = 3
THREADS = 2


@pytest.fixture(scope="module")
def server() -> Iterator[support.Server]:
    instance = support.start("plain_wsgi:application", workers=WORKERS, threads=THREADS)

    try:
        yield instance
    finally:
        instance.stop()


def test_hello(server: support.Server) -> None:
    response = server.get("/")
    assert response.status == 200
    assert response.body == b"Hello, world!\n"
    assert response.header("Content-Length") == "14"
    assert response.header("Server") == "muru"


def test_requests_spread_over_forked_workers(server: support.Server) -> None:
    pids = set()

    for _ in range(WORKERS * 10):
        response = server.get("/info")
        assert response.status == 200
        pids.add(json.loads(response.body)["pid"])

    assert len(pids) == WORKERS


def test_workers_have_collection_enabled_and_frozen_preload(server: support.Server) -> None:
    info = json.loads(server.get("/info").body)
    assert info["gc_enabled"] is True
    assert info["gc_frozen"] > 1000
    assert info["python"] == sys.version.split()[0]


def test_environ(server: support.Server) -> None:
    response = server.get("/info?a=1&b=two", headers={"X-Custom": "yes", "Host": "example.test"})
    environ = json.loads(response.body)["environ"]
    assert environ["REQUEST_METHOD"] == "GET"
    assert environ["PATH_INFO"] == "/info"
    assert environ["QUERY_STRING"] == "a=1&b=two"
    assert environ["HTTP_X_CUSTOM"] == "yes"
    assert environ["HTTP_HOST"] == "example.test"
    assert environ["SERVER_PROTOCOL"] == "HTTP/1.1"
    assert environ["wsgi.url_scheme"] == "http"
    assert environ["wsgi.multiprocess"] is True
    assert environ["wsgi.multithread"] is True
    assert environ["REMOTE_ADDR"] == "127.0.0.1"


def test_echo_body(server: support.Server) -> None:
    payload = os.urandom(300_000)
    response = server.post("/echo", payload, headers={"Content-Type": "application/octet-stream"})
    assert response.status == 200
    assert response.header("Content-Type") == "application/octet-stream"
    assert response.body == payload


def test_input_iteration(server: support.Server) -> None:
    response = server.post("/lines", b"one\ntwo\nthree")
    assert json.loads(response.body) == ["one\n", "two\n", "three"]


def test_streaming_chunks_are_coalesced(server: support.Server) -> None:
    response = server.get("/chunks?n=5000")
    assert response.status == 200
    assert response.header("Transfer-Encoding") == "chunked"
    assert len(response.body) == 5000 * 16
    assert response.body.endswith(b"%015d\n" % 4999)


def test_large_body(server: support.Server) -> None:
    response = server.get("/large")
    assert response.status == 200
    assert len(response.body) == 1024 * 1024


def test_write_callable(server: support.Server) -> None:
    assert server.get("/write").body == b"written and yielded\n"


def test_application_error_is_a_500(server: support.Server) -> None:
    response = server.get("/error")
    assert response.status == 500
    assert b"Internal Server Error" in response.body


def test_not_found(server: support.Server) -> None:
    assert server.get("/missing").status == 404


def test_keep_alive_connection(server: support.Server) -> None:
    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=30)

    try:
        for _ in range(20):
            connection.request("GET", "/")
            response = connection.getresponse()
            assert response.status == 200
            assert response.read() == b"Hello, world!\n"
    finally:
        connection.close()


def test_concurrent_requests(server: support.Server) -> None:
    def worker(_index: int) -> int:
        response = server.get("/sleep?seconds=0.05")
        assert response.body == b"slept"
        return response.status

    with ThreadPoolExecutor(max_workers=12) as pool:
        statuses = list(pool.map(worker, range(48)))
    assert statuses == [200] * 48


def test_thread_locals_survive_across_requests(server: support.Server) -> None:
    # Section 6.5: application threads are long-lived, so a threading.local
    # value set by one request is still there for a later request on the
    # same thread. Six threads serve these requests, so counts must climb.

    counts = []
    for _ in range(WORKERS * THREADS * 8):
        counts.append(json.loads(server.get("/threadlocal").body)["count"])
    assert max(counts) >= 4


def test_each_request_gets_a_fresh_context(server: support.Server) -> None:
    # Section 6.5: a context variable set during one request is not visible
    # to the next request on the same thread.

    for _ in range(WORKERS * THREADS * 4):
        assert json.loads(server.get("/context").body) == {"inherited": None}


def test_unread_body_keeps_the_connection_alive(server: support.Server) -> None:
    # Section 4.6: a response sent before the request body was read, with
    # the rest of the body under the discard limit, leaves the client
    # connection usable and the worker link connection back in its pool.

    payload = b"x" * (512 * 1024)

    with socket.create_connection(("127.0.0.1", server.port), timeout=30) as sock:
        for _ in range(3):
            sock.sendall(
                b"POST /ignore HTTP/1.1\r\nHost: test\r\nContent-Length: %d\r\n\r\n" % len(payload)
            )
            sock.sendall(payload)
            assert read_response(sock) == b"ignored\n"

        sock.sendall(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        assert read_response(sock) == b"Hello, world!\n"

    # Every slot must still be free: more requests than slots succeed at once.

    with ThreadPoolExecutor(max_workers=12) as pool:
        statuses = list(pool.map(lambda _i: server.get("/").status, range(24)))
    assert statuses == [200] * 24


def read_response(sock: socket.socket) -> bytes:
    """Read one HTTP/1.1 response with a Content-Length and return its body."""

    data = b""

    while b"\r\n\r\n" not in data:
        chunk = sock.recv(65536)
        assert chunk, "the connection was closed"
        data += chunk

    head, _, body = data.partition(b"\r\n\r\n")
    status_line = head.split(b"\r\n", 1)[0]
    assert status_line.startswith(b"HTTP/1.1 200")
    length = int(
        next(
            line.split(b":")[1]
            for line in head.split(b"\r\n")
            if line.lower().startswith(b"content-length:")
        )
    )

    while len(body) < length:
        chunk = sock.recv(65536)
        assert chunk, "the connection was closed"
        body += chunk

    return body
