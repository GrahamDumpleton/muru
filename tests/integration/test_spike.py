"""Phase 0 integration tests: muructl run with a preloaded, forked WSGI application.

These need the muructl binary (target/debug/muructl, or MURUCTL) and the
extension built into the test interpreter (`just dev`). They skip otherwise.
"""

from __future__ import annotations

import http.client
import json
import os
import signal
import socket
import subprocess
import sys
import time
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures" / "apps"
WORKERS = 3


def muructl_path() -> Path | None:
    candidate = Path(os.environ.get("MURUCTL", ROOT / "target" / "debug" / "muructl"))
    return candidate if candidate.is_file() else None


def extension_available() -> bool:
    try:
        from muru import _native  # noqa: F401
    except ImportError:
        return False
    return True


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def wait_for_port(port: int, process: subprocess.Popen[bytes], timeout: float = 60.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"muructl exited early with {process.returncode}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("muructl did not start listening in time")


@pytest.fixture(scope="module")
def server() -> Iterator[int]:
    muructl = muructl_path()
    if muructl is None:
        pytest.skip("muructl is not built (cargo build -p muructl)")
    if not extension_available():
        pytest.skip("the muru extension is not built into this interpreter (just dev)")
    port = free_port()
    process = subprocess.Popen(
        [
            str(muructl),
            "run",
            "plain_wsgi:application",
            "--bind",
            f"127.0.0.1:{port}",
            "--workers",
            str(WORKERS),
            "--threads",
            "2",
            "--python",
            sys.executable,
            "--chdir",
            str(FIXTURES),
        ],
    )
    try:
        wait_for_port(port, process)
        yield port
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def request(
    port: int,
    method: str,
    path: str,
    body: bytes | None = None,
    headers: dict[str, str] | None = None,
) -> tuple[int, dict[str, str], bytes]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    try:
        connection.request(method, path, body=body, headers=headers or {})
        response = connection.getresponse()
        headers = {name.lower(): value for name, value in response.getheaders()}
        return response.status, headers, response.read()
    finally:
        connection.close()


def test_hello(server: int) -> None:
    status, headers, body = request(server, "GET", "/")
    assert status == 200
    assert body == b"Hello, world!\n"
    assert headers["content-length"] == "14"
    assert headers["server"] == "muru"


def test_requests_spread_over_forked_workers(server: int) -> None:
    pids = set()
    for _ in range(WORKERS * 10):
        status, _headers, body = request(server, "GET", "/info")
        assert status == 200
        pids.add(json.loads(body)["pid"])
    assert len(pids) == WORKERS


def test_workers_have_collection_enabled_and_frozen_preload(server: int) -> None:
    _status, _headers, body = request(server, "GET", "/info")
    info = json.loads(body)
    assert info["gc_enabled"] is True
    assert info["gc_frozen"] > 1000
    assert info["python"] == sys.version.split()[0]


def test_environ(server: int) -> None:
    _status, _headers, body = request(
        server, "GET", "/info?a=1&b=two", headers={"X-Custom": "yes", "Host": "example.test"}
    )
    environ = json.loads(body)["environ"]
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


def test_echo_body(server: int) -> None:
    payload = os.urandom(300_000)
    status, headers, body = request(
        server, "POST", "/echo", body=payload, headers={"Content-Type": "application/octet-stream"}
    )
    assert status == 200
    assert headers["content-type"] == "application/octet-stream"
    assert body == payload


def test_input_iteration(server: int) -> None:
    _status, _headers, body = request(server, "POST", "/lines", body=b"one\ntwo\nthree")
    assert json.loads(body) == ["one\n", "two\n", "three"]


def test_streaming_chunks_are_coalesced(server: int) -> None:
    status, headers, body = request(server, "GET", "/chunks?n=5000")
    assert status == 200
    assert headers.get("transfer-encoding") == "chunked"
    assert len(body) == 5000 * 16
    assert body.endswith(b"%015d\n" % 4999)


def test_large_body(server: int) -> None:
    status, _headers, body = request(server, "GET", "/large")
    assert status == 200
    assert len(body) == 1024 * 1024


def test_write_callable(server: int) -> None:
    _status, _headers, body = request(server, "GET", "/write")
    assert body == b"written and yielded\n"


def test_application_error_is_a_500(server: int) -> None:
    status, _headers, body = request(server, "GET", "/error")
    assert status == 500
    assert b"Internal Server Error" in body


def test_not_found(server: int) -> None:
    status, _headers, _body = request(server, "GET", "/missing")
    assert status == 404


def test_keep_alive_connection(server: int) -> None:
    connection = http.client.HTTPConnection("127.0.0.1", server, timeout=30)
    try:
        for _ in range(20):
            connection.request("GET", "/")
            response = connection.getresponse()
            assert response.status == 200
            assert response.read() == b"Hello, world!\n"
    finally:
        connection.close()


def test_concurrent_requests(server: int) -> None:
    def worker(_index: int) -> int:
        status, _headers, body = request(server, "GET", "/sleep?seconds=0.05")
        assert body == b"slept"
        return status

    with ThreadPoolExecutor(max_workers=12) as pool:
        statuses = list(pool.map(worker, range(48)))
    assert statuses == [200] * 48


def test_thread_locals_survive_across_requests(server: int) -> None:
    # Section 6.5: application threads are long-lived, so a threading.local
    # value set by one request is still there for a later request on the
    # same thread. Six threads serve these requests, so counts must climb.
    counts = []
    for _ in range(WORKERS * 2 * 8):
        _status, _headers, body = request(server, "GET", "/threadlocal")
        counts.append(json.loads(body)["count"])
    assert max(counts) >= 4


def test_each_request_gets_a_fresh_context(server: int) -> None:
    # Section 6.5: a context variable set during one request is not visible
    # to the next request on the same thread.
    for _ in range(WORKERS * 2 * 4):
        _status, _headers, body = request(server, "GET", "/context")
        assert json.loads(body) == {"inherited": None}


def test_unread_body_keeps_the_connection_alive(server: int) -> None:
    # Section 4.6: a response sent before the request body was read, with
    # the rest of the body under the discard limit, leaves the client
    # connection usable and the worker link connection back in its pool.
    payload = b"x" * (512 * 1024)
    with socket.create_connection(("127.0.0.1", server), timeout=30) as sock:
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
        statuses = list(pool.map(lambda _i: request(server, "GET", "/")[0], range(24)))
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
