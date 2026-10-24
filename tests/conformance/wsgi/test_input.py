"""wsgi.input: read, readline, readlines, iteration and request bodies."""

from __future__ import annotations

import hashlib
import os

import pytest
from support.server import Server, text

AREA = "/input"


@pytest.mark.parametrize(
    ("body", "expected"),
    [
        (b"hello world", "data=hello world"),
        (b"", "data="),
        (b"line1\nline2\nline3", "data=line1\nline2\nline3"),
        (bytes(range(256)), "data=" + bytes(range(256)).decode("utf-8", "surrogateescape")),
    ],
)
def test_read_all(server: Server, body: bytes, expected: str) -> None:
    assert text(server.post(f"{AREA}/read-all", body)) == expected


def test_read_sized(server: Server) -> None:
    assert (
        text(server.post(f"{AREA}/read-sized", b"abcdefghijklmno")) == "first=abcdefghij,rest=klmno"
    )


def test_read_in_a_loop(server: Server) -> None:
    body = b"this is a long body that spans chunks"
    assert text(server.post(f"{AREA}/read-chunks", body)) == f"chunks=6,data={body.decode()}"


def test_read_zero_does_not_consume(server: Server) -> None:
    assert text(server.post(f"{AREA}/read-zero", b"hello")) == "zero_len=0,rest=hello"


def test_read_past_eof_returns_empty(server: Server) -> None:
    assert (
        text(server.post(f"{AREA}/read-past-eof", b"abc")) == "first_len=3,second_len=0,third_len=0"
    )


def test_read_more_than_the_body(server: Server) -> None:
    assert text(server.post(f"{AREA}/read-more-than-body", b"short")) == "data=short"


@pytest.mark.parametrize(
    ("body", "expected"),
    [
        (b"line1\nline2\nline3", "count=3,lines=line1|line2|line3"),
        (b"a\nb\nc\n", "count=3,lines=a|b|c"),
        (b"\n\n", "count=2,lines=|"),
        (b"", "count=0,lines="),
    ],
)
def test_readline(server: Server, body: bytes, expected: str) -> None:
    assert text(server.post(f"{AREA}/readline", body)) == expected


@pytest.mark.parametrize(
    ("body", "expected"),
    [
        (b"abcdefghijklm", "first=abcde,second=fghij,rest=klm"),
        (b"ab\ncdefghijklm", "first=ab\n,second=cdefg,rest=hijklm"),
    ],
)
def test_readline_with_a_limit(server: Server, body: bytes, expected: str) -> None:
    assert text(server.post(f"{AREA}/readline-sized", body)) == expected


def test_readlines(server: Server) -> None:
    assert text(server.post(f"{AREA}/readlines", b"aa\nbb\ncc")) == "count=3,lines=aa|bb|cc"


def test_readlines_with_a_hint_stops_early(server: Server) -> None:
    assert text(server.post(f"{AREA}/readlines-hint", b"xx\nyy\nzz")) == "count=1,lines=xx"


def test_iteration_yields_lines(server: Server) -> None:
    assert text(server.post(f"{AREA}/iterate", b"p\nq\nr")) == "count=3,lines=p|q|r"


@pytest.mark.parametrize("size", [0, 1, 11, 1024, 8192, 65536, 300000, 2 * 1024 * 1024])
def test_bodies_arrive_intact(server: Server, size: int) -> None:
    body = os.urandom(size)
    expected = f"len={size} sha256={hashlib.sha256(body).hexdigest()}"
    assert text(server.post(f"{AREA}/digest", body)) == expected


def test_chunked_request_body_is_readable(server: Server) -> None:
    request = (
        b"POST /input/read-all HTTP/1.1\r\nHost: t\r\nConnection: close\r\n"
        b"Transfer-Encoding: chunked\r\n\r\n"
        b"7\r\nchunked\r\n5\r\n-body\r\n0\r\n\r\n"
    )
    response = server.raw(request)
    assert response.startswith(b"HTTP/1.1 200")
    assert response.endswith(b"data=chunked-body;end")


def test_chunked_request_body_in_many_reads(server: Server) -> None:
    body = b"chunked-long-body-that-spans-many-small-reads"
    request = (
        b"POST /input/read-chunks HTTP/1.1\r\nHost: t\r\nConnection: close\r\n"
        b"Transfer-Encoding: chunked\r\n\r\n"
        + b"".join(
            b"%x\r\n%s\r\n" % (len(body[i : i + 10]), body[i : i + 10])
            for i in range(0, len(body), 10)
        )
        + b"0\r\n\r\n"
    )
    response = server.raw(request)
    assert response.endswith(b"chunks=7,data=" + body + b";end")


def test_input_is_terminated_and_content_length_is_set(server: Server) -> None:
    body = text(server.post(f"{AREA}/content-length-keys", b"12345"))
    assert body == "present=True,value=5,terminated=True,len=5"


def test_chunked_body_is_terminated(server: Server) -> None:
    # Section 4.6: with the default buffering the edge also sets
    # CONTENT_LENGTH for chunked bodies. Until the edge buffers, only the
    # PEP 3333 requirement is asserted: the stream is terminated and the
    # body is complete.
    request = (
        b"POST /input/content-length-keys HTTP/1.1\r\nHost: t\r\nConnection: close\r\n"
        b"Transfer-Encoding: chunked\r\n\r\n"
        b"5\r\n12345\r\n0\r\n\r\n"
    )
    response = server.raw(request)
    assert b"terminated=True,len=5;end" in response


def test_unread_body_does_not_break_the_next_request(server: Server) -> None:
    for _ in range(server.workers * server.threads * 2):
        assert text(server.post(f"{AREA}/unread", b"x" * 100000)) == "ignored"
    assert server.get("/response/list").status == 200


@pytest.mark.parametrize("size", [10, 1024 * 1024])
def test_reading_after_writing_does_not_deadlock(server: Server, size: int) -> None:
    response = server.post(f"{AREA}/read-after-response", b"y" * size)
    assert response.body == b"started-read=%d" % size
