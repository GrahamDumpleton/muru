"""Request bodies at the edge: Expect: 100-continue, chunked bodies and the limit."""

from __future__ import annotations

import hashlib
import socket

import pytest
from support.server import Server, text

LIMIT = 10 * 1024 * 1024


def connect(server: Server, timeout: float = 10.0) -> socket.socket:
    return socket.create_connection(("127.0.0.1", server.port), timeout=timeout)


def recv_head(sock: socket.socket) -> bytes:
    """Read one head, up to and including its blank line."""

    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(65536)
        assert chunk, f"the connection closed after {data!r}"
        data += chunk
    return data


def recv_until_closed(sock: socket.socket) -> bytes:
    chunks = []
    while True:
        try:
            chunk = sock.recv(65536)
        except (TimeoutError, ConnectionResetError):
            break
        if not chunk:
            break
        chunks.append(chunk)
    return b"".join(chunks)


def chunked(*pieces: bytes) -> bytes:
    return b"".join(b"%x\r\n%s\r\n" % (len(piece), piece) for piece in pieces) + b"0\r\n\r\n"


def test_continue_is_sent_once_the_application_reads(server: Server) -> None:
    with connect(server) as sock:
        sock.sendall(
            b"POST /input/read-all HTTP/1.1\r\nHost: t\r\nContent-Length: 11\r\n"
            b"Expect: 100-continue\r\nConnection: close\r\n\r\n"
        )
        assert recv_head(sock) == b"HTTP/1.1 100 Continue\r\n\r\n"
        sock.sendall(b"hello world")
        response = recv_until_closed(sock)
    assert response.startswith(b"HTTP/1.1 200")
    assert b"data=hello world;end" in response


def test_no_continue_when_the_application_answers_without_reading(server: Server) -> None:
    # Section 4.6: the body is never asked for, so it never crosses the
    # link, and because the client may still send it the connection is
    # closed after the response, as Apache does.

    with connect(server) as sock:
        sock.sendall(
            b"POST /input/unread HTTP/1.1\r\nHost: t\r\nContent-Length: 11\r\n"
            b"Expect: 100-continue\r\n\r\n"
        )
        response = recv_until_closed(sock)
    assert response.startswith(b"HTTP/1.1 200"), response
    assert b"100 Continue" not in response
    assert b"ignored;end" in response
    assert b"connection: close" in response.lower()


def test_continue_is_sent_before_a_chunked_body_is_buffered(server: Server) -> None:
    with connect(server) as sock:
        sock.sendall(
            b"POST /input/read-all HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n"
            b"Expect: 100-continue\r\nConnection: close\r\n\r\n"
        )
        assert recv_head(sock) == b"HTTP/1.1 100 Continue\r\n\r\n"
        sock.sendall(chunked(b"hello ", b"world"))
        response = recv_until_closed(sock)
    assert b"data=hello world;end" in response


def test_chunked_body_is_buffered_and_content_length_set(server: Server) -> None:
    with connect(server) as sock:
        sock.sendall(
            b"POST /input/content-length-keys HTTP/1.1\r\nHost: t\r\n"
            b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            + chunked(b"hello", b" ", b"world")
        )
        response = recv_until_closed(sock)
    assert b"present=True,value=11,terminated=True,len=11;end" in response


def test_empty_chunked_body(server: Server) -> None:
    with connect(server) as sock:
        sock.sendall(
            b"POST /input/content-length-keys HTTP/1.1\r\nHost: t\r\n"
            b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n" + chunked()
        )
        response = recv_until_closed(sock)
    assert b"present=True,value=0,terminated=True,len=0;end" in response


def test_large_chunked_body_arrives_intact(server: Server) -> None:
    pieces = [bytes([index % 256]) * 65536 for index in range(48)]
    body = b"".join(pieces)
    with connect(server, timeout=30) as sock:
        sock.sendall(
            b"POST /input/digest HTTP/1.1\r\nHost: t\r\n"
            b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        for piece in pieces:
            sock.sendall(b"%x\r\n%s\r\n" % (len(piece), piece))
        sock.sendall(b"0\r\n\r\n")
        response = recv_until_closed(sock)
    expected = f"len={len(body)} sha256={hashlib.sha256(body).hexdigest()};end".encode()
    assert expected in response


@pytest.mark.parametrize("key", ["HTTP_CONNECTION", "HTTP_TRANSFER_ENCODING", "HTTP_KEEP_ALIVE"])
def test_hop_by_hop_headers_do_not_reach_the_application(server: Server, key: str) -> None:
    with connect(server) as sock:
        sock.sendall(
            b"POST /environ/has?key=%s HTTP/1.1\r\nHost: t\r\nKeep-Alive: timeout=5\r\n"
            b"Transfer-Encoding: chunked\r\nConnection: close, keep-alive\r\n\r\n"
            % key.encode()
            + chunked(b"x")
        )
        response = recv_until_closed(sock)
    assert b"present=NO;end" in response


def test_headers_named_by_connection_are_dropped(server: Server) -> None:
    response = server.get(
        "/environ/has?key=HTTP_X_PER_HOP", headers={"X-Per-Hop": "1", "Connection": "x-per-hop"}
    )
    assert text(response) == "present=NO"


def test_declared_length_over_the_limit_is_refused_before_reading(server: Server) -> None:
    with connect(server) as sock:
        sock.sendall(
            b"POST /input/read-all HTTP/1.1\r\nHost: t\r\nContent-Length: %d\r\n\r\n" % (LIMIT + 1)
        )

        # No body is sent: the answer must come without it.

        response = recv_until_closed(sock)
    assert response.startswith(b"HTTP/1.1 413"), response
    assert b"connection: close" in response.lower()
    assert b"max_body_size" in response
    assert server.get("/response/list").status == 200


def test_declared_length_at_the_limit_is_accepted(server: Server) -> None:
    body = b"y" * LIMIT
    response = server.post("/input/digest", body)
    assert text(response) == f"len={LIMIT} sha256={hashlib.sha256(body).hexdigest()}"


def test_chunked_body_over_the_limit_is_refused(server: Server) -> None:
    # The edge answers 413 as soon as the limit is passed and closes the
    # connection. A client still sending may see the reset before the
    # response, so either counts, as long as the server is fine afterwards.

    piece = b"z" * 65536
    response = b""
    with connect(server, timeout=30) as sock:
        sock.sendall(
            b"POST /input/read-all HTTP/1.1\r\nHost: t\r\n"
            b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        try:
            for _ in range(LIMIT // len(piece) + 2):
                sock.sendall(b"%x\r\n%s\r\n" % (len(piece), piece))
            sock.sendall(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError):
            pass
        try:
            response = recv_until_closed(sock)
        except OSError:
            pass
    if response:
        assert response.startswith(b"HTTP/1.1 413"), response
    assert server.get("/response/list").status == 200
