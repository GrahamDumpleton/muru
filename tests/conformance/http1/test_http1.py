"""HTTP/1.1 parsing at the edge: malformed and ambiguous requests are refused.

The request smuggling corpus of section 15.1: conflicting framing headers,
invalid lengths, obsolete line folding and bad request lines must never
reach the application, and the connection is closed afterwards because the
parse state is suspect (section 4.6).
"""

from __future__ import annotations

import socket

import pytest
from support.server import Server


def exchange(server: Server, data: bytes, timeout: float = 5.0) -> bytes:
    with socket.create_connection(("127.0.0.1", server.port), timeout=timeout) as sock:
        sock.sendall(data)
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


def status_of(response: bytes) -> int | None:
    if not response.startswith(b"HTTP/1."):
        return None
    return int(response.split(b" ", 2)[1])


REFUSED = [
    pytest.param(
        b"POST /input/read-all HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\n"
        b"Content-Length: 6\r\n\r\nhello",
        id="conflicting-content-lengths",
    ),
    pytest.param(
        b"POST /input/read-all HTTP/1.1\r\nHost: t\r\nContent-Length: abc\r\n\r\n",
        id="non-numeric-content-length",
    ),
    pytest.param(
        b"POST /input/read-all HTTP/1.1\r\nHost: t\r\nContent-Length: -1\r\n\r\n",
        id="negative-content-length",
    ),
    pytest.param(
        b"GET /response/list HTTP/1.1\r\nHost: t\r\nX-Folded: a\r\n b\r\n\r\n",
        id="obsolete-line-folding",
    ),
    pytest.param(b"GET /response/list HTTP/9.9\r\nHost: t\r\n\r\n", id="unknown-version"),
    pytest.param(b"GET  HTTP/1.1\r\nHost: t\r\n\r\n", id="missing-target"),
    pytest.param(
        b"GET /response/list HTTP/1.1\r\nHost: t\r\nBad Header: x\r\n\r\n",
        id="space-in-header-name",
    ),
    pytest.param(
        b"GET /response/list HTTP/1.1\r\nHost: t\r\nX-Ctl: a\x01b\r\n\r\n",
        id="control-byte-in-value",
    ),
    pytest.param(
        b"GET /response/list HTTP/1.1\r\nHost: t\r\n" + b"X-Big: " + b"a" * 70000 + b"\r\n\r\n",
        id="head-too-large",
    ),
    pytest.param(
        b"GET /response/list HTTP/1.1\r\nHost: t\r\n"
        + b"".join(b"X-%d: y\r\n" % i for i in range(150))
        + b"\r\n",
        id="too-many-headers",
    ),
]


@pytest.mark.parametrize("request_bytes", REFUSED)
def test_malformed_requests_never_reach_the_application(
    server: Server, request_bytes: bytes
) -> None:
    server.mark_log()
    response = exchange(server, request_bytes)
    status = status_of(response)

    # hyper answers 400 (or 431 for an oversized head) and closes, or just
    # closes when it cannot even frame a response.

    assert status in (None, 400, 431), response[:200]
    assert b"chunk1-chunk2-chunk3" not in response
    assert b"data=" not in response
    assert "GET /response/list" not in server.log_since_mark()
    assert "POST /input/read-all" not in server.log_since_mark()


def test_transfer_encoding_wins_over_content_length_and_closes(server: Server) -> None:
    # RFC 9112 section 6.3: Transfer-Encoding overrides Content-Length. hyper
    # applies that and closes the connection afterwards, so the bytes after
    # the chunked body can never be read as a smuggled second request.

    response = exchange(
        server,
        b"POST /input/read-all HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\n"
        b"Transfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /response/list HTTP/1.1\r\n\r\n",
    )
    assert status_of(response) == 200
    assert response.endswith(b"data=;end")
    assert b"connection: close" in response.lower()
    assert b"chunk1-chunk2-chunk3" not in response


def test_bare_line_feeds_are_tolerated(server: Server) -> None:
    # Like most servers, hyper accepts a bare LF as a line ending.

    response = exchange(server, b"GET /environ/has?key=X HTTP/1.1\nHost: t\nConnection: close\n\n")
    assert status_of(response) == 200
    assert response.endswith(b"present=NO;end")


def test_chunked_body_with_bad_chunk_size_is_refused(server: Server) -> None:
    response = exchange(
        server,
        b"POST /input/read-all HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n"
        b"zz\r\nhello\r\n0\r\n\r\n",
    )
    assert status_of(response) in (None, 400), response[:200]


def test_pipelined_requests_are_answered_in_order(server: Server) -> None:
    response = exchange(
        server,
        b"GET /environ/get?key=REQUEST_METHOD HTTP/1.1\r\nHost: t\r\n\r\n"
        b"GET /environ/has?key=X HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    first = response.index(b"value=GET;end")
    second = response.index(b"present=NO;end")
    assert first < second


def test_http10_without_host_is_served(server: Server) -> None:
    response = exchange(server, b"GET /response/list HTTP/1.0\r\n\r\n")
    assert response.startswith(b"HTTP/1.0 200")
    assert response.endswith(b"chunk1-chunk2-chunk3")


def test_http11_without_host_is_tolerated(server: Server) -> None:
    # RFC 9112 asks for 400 here; hyper serves the request, and the
    # application sees no HTTP_HOST. Recorded so a change is noticed.

    response = exchange(
        server, b"GET /environ/has?key=HTTP_HOST HTTP/1.1\r\nConnection: close\r\n\r\n"
    )
    assert status_of(response) == 200
    assert response.endswith(b"present=NO;end")


def test_absolute_form_target_is_served(server: Server) -> None:
    response = exchange(
        server,
        b"GET http://example.test/environ/get?key=HTTP_HOST HTTP/1.1\r\nHost: example.test\r\n"
        b"Connection: close\r\n\r\n",
    )
    assert status_of(response) == 200
    assert b"value=example.test;end" in response
