"""Response iteration: iterables, close(), errors and Content-Length."""

from __future__ import annotations

import http.client

import pytest
from support.server import Server

AREA = "/response"


def assert_truncated(response: bytes, expected: bytes) -> None:
    """A response that failed after output started must not end cleanly.

    What reaches the client before the failure depends on buffering in the
    edge: the head and the earlier output, or nothing at all when the
    failure arrives before the first flush. Either way the chunked body
    must not carry its terminator, so a client can tell it was cut short.
    """

    if response:
        assert response.startswith(b"HTTP/1.1 200"), response
        assert expected in response, response
        assert not response.endswith(b"0\r\n\r\n"), response


@pytest.mark.parametrize(
    ("case", "expected"),
    [
        ("list", b"chunk1-chunk2-chunk3"),
        ("tuple", b"tuple-atuple-b"),
        ("generator", b"gen-one-gen-two-gen-three"),
        ("empty-chunks", b"ab"),
    ],
)
def test_iterables_are_sent_in_order(server: Server, case: str, expected: bytes) -> None:
    response = server.get(f"{AREA}/{case}")
    assert response.status == 200
    assert response.body == expected


@pytest.mark.parametrize("case", ["empty-list", "empty-generator"])
def test_empty_iterables_give_an_empty_body(server: Server, case: str) -> None:
    response = server.get(f"{AREA}/{case}")
    assert response.status == 200
    assert response.body == b""


def test_close_is_called_on_a_custom_iterable(server: Server) -> None:
    server.mark_log()
    assert server.get(f"{AREA}/iterable-with-close").body == b"iter-a-iter-b"
    assert server.wait_for_log("MARKER_ITER_CLOSE")


def test_generator_finally_runs_after_completion(server: Server) -> None:
    server.mark_log()
    assert server.get(f"{AREA}/generator-finally").body == b"gen-fin-a-gen-fin-b"
    assert server.wait_for_log("MARKER_GEN_FINALLY")


def test_close_reaches_a_paused_generator(server: Server) -> None:
    server.mark_log()
    response = server.raw(
        b"GET /response/close-via-generator-exit HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
    )
    assert_truncated(response, b"first-ok")
    assert server.wait_for_log("MARKER_CLOSE_GEXIT")


def test_close_raising_is_logged_and_the_response_stands(server: Server) -> None:
    server.mark_log()
    response = server.get(f"{AREA}/close-raises")
    assert response.status == 200
    assert response.body == b"before-close-error"
    assert server.wait_for_log("close-boom")


def test_exception_before_output_is_a_500(server: Server) -> None:
    server.mark_log()
    response = server.get(f"{AREA}/raise-first")
    assert response.status == 500
    assert server.wait_for_log("raise-first-boom")


def test_exception_before_start_response_is_a_500(server: Server) -> None:
    server.mark_log()
    response = server.get(f"{AREA}/raise-before-start")
    assert response.status == 500
    assert server.wait_for_log("raise-before-start-boom")


def test_exception_after_output_truncates_the_response(server: Server) -> None:
    server.mark_log()
    response = server.raw(
        b"GET /response/raise-midway HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
    )
    assert_truncated(response, b"before-boom")
    assert server.wait_for_log("raise-midway-boom")


def test_generator_finally_runs_when_it_raises(server: Server) -> None:
    server.mark_log()
    server.raw(b"GET /response/raise-with-finally HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
    assert server.wait_for_log("MARKER_RAISE_FINALLY")


@pytest.mark.parametrize("case", ["raise-systemexit", "raise-keyboardinterrupt"])
def test_base_exceptions_do_not_kill_the_worker(server: Server, case: str) -> None:
    server.mark_log()
    response = server.get(f"{AREA}/{case}")
    assert response.status == 500

    # The worker that served it is still serving.

    for _ in range(server.workers * 4):
        assert server.get(f"{AREA}/list").status == 200
    assert "restarting" not in server.log_since_mark()


def test_non_bytes_first_item_is_a_500(server: Server) -> None:
    assert server.get(f"{AREA}/non-bytes-first").status == 500


def test_non_bytes_later_item_truncates(server: Server) -> None:
    server.mark_log()
    response = server.raw(
        b"GET /response/non-bytes-midway HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
    )
    assert_truncated(response, b"bytes-ok")
    assert server.wait_for_log("TypeError")


def test_bytearray_items_are_rejected(server: Server) -> None:
    assert server.get(f"{AREA}/bytearray-item").status == 500


def test_non_iterable_result_is_a_500(server: Server) -> None:
    assert server.get(f"{AREA}/not-iterable").status == 500


def test_content_length_exact(server: Server) -> None:
    response = server.get(f"{AREA}/content-length-exact")
    assert response.header("Content-Length") == "10"
    assert response.body == b"exactly10!"


@pytest.mark.parametrize(
    ("case", "expected"),
    [("content-length-over", b"this-"), ("content-length-over-chunks", b"abcdefg")],
)
def test_output_beyond_content_length_is_dropped(
    server: Server, case: str, expected: bytes
) -> None:
    response = server.get(f"{AREA}/{case}")
    assert response.status == 200
    assert response.body == expected

    # And the connection is usable afterwards, so the framing was right.

    assert server.get(f"{AREA}/list").status == 200


def test_output_short_of_content_length_fails_the_response(server: Server) -> None:
    server.mark_log()

    # The connection is closed without a complete response: hyper may have
    # sent the head and part of the body, or nothing at all.

    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=30)

    try:
        connection.request("GET", f"{AREA}/content-length-under")
        with pytest.raises((http.client.IncompleteRead, http.client.RemoteDisconnected)):
            connection.getresponse().read()
    finally:
        connection.close()

    assert server.wait_for_log("Content-Length")


def test_content_length_zero(server: Server) -> None:
    response = server.get(f"{AREA}/content-length-zero")
    assert response.status == 200
    assert response.header("Content-Length") == "0"
    assert response.body == b""


def test_many_chunks_arrive_complete(server: Server) -> None:
    response = server.get(f"{AREA}/many-chunks")
    assert response.header("Transfer-Encoding") == "chunked"
    assert response.body.startswith(b"chunk000-")
    assert response.body.endswith(b"chunk099-")
    assert len(response.body) == 100 * 9


def test_slow_chunks_are_flushed_as_they_come(server: Server) -> None:
    assert server.get(f"{AREA}/slow-chunks").body == b"slow0-slow1-slow2-"


@pytest.mark.parametrize("size", [0, 1, 4095, 4096, 4097, 65536, 1048576, 3000000])
def test_large_bodies_are_intact(server: Server, size: int) -> None:
    response = server.get(f"{AREA}/large?size={size}")
    assert len(response.body) == size
    assert response.body == bytes(range(256)) * (size // 256) + bytes(range(size % 256))


def test_no_content_type_is_allowed(server: Server) -> None:
    response = server.get(f"{AREA}/no-content-type")
    assert response.status == 200
    assert response.body == b"no content type"


@pytest.mark.parametrize("case", ["status-204", "status-304"])
def test_bodiless_statuses(server: Server, case: str) -> None:
    response = server.get(f"{AREA}/{case}")
    assert response.status == int(case.split("-")[1])
    assert response.body == b""
    assert server.get(f"{AREA}/list").status == 200
