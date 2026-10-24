"""HTTP messages: what receive() gives and what send() accepts."""

from __future__ import annotations

import http.client
import json

import pytest
from support.server import Server


def test_empty_body_is_one_request_message_then_disconnect(server: Server) -> None:
    server.mark_log()
    messages = json.loads(server.get("/messages").body)

    assert messages == [{"type": "http.request", "size": 0, "more_body": False}]
    assert server.wait_for_log("after response http.disconnect")


def test_body_messages_chain_with_more_body(server: Server) -> None:
    body = b"x" * 300_000
    messages = json.loads(server.post("/messages", body).body)
    requests = [message for message in messages if message["type"] == "http.request"]

    assert sum(message["size"] for message in requests) == len(body)
    assert all(message["more_body"] for message in requests[:-1])
    assert requests[-1]["more_body"] is False
    assert len(requests) == len(messages)


def test_state_is_copied_per_request(server: Server) -> None:
    first = json.loads(server.get("/state").body)
    second = json.loads(server.get("/state").body)

    assert first["seen"] is None
    assert second["seen"] is None
    assert first["pid"] == second["pid"]


def test_response_without_length_is_chunked(server: Server) -> None:
    response = server.get("/chunked")

    assert response.status == 200
    assert response.header("transfer-encoding") == "chunked"
    assert response.body == b"012"


def test_body_beyond_content_length_is_dropped(server: Server) -> None:
    response = server.get("/over-length")

    assert response.header("content-length") == "3"
    assert response.body == b"123"


def test_204_has_no_body(server: Server) -> None:
    response = server.get("/no-body-status")

    assert response.status == 204
    assert response.body == b""


def test_head_request_gets_headers_without_the_body(server: Server) -> None:
    response = server.request("HEAD", "/head")

    assert response.status == 200
    assert response.header("x-method") == "HEAD"
    assert response.header("content-length") == "13"
    assert response.body == b""


def test_duplicate_headers_are_kept(server: Server) -> None:
    response = server.get("/duplicate-headers")

    assert response.header_values("x-dup") == ["a", "b"]


@pytest.mark.parametrize(
    ("path", "message"),
    [
        ("/bad-status-type", "status must be an int"),
        ("/bad-headers-type", "header names and values must be bytes"),
        ("/unknown-message", 'unexpected ASGI message type "http.response.whatever"'),
    ],
)
def test_invalid_send_is_a_500_before_output(server: Server, path: str, message: str) -> None:
    server.mark_log()
    response = server.get(path, headers={"Accept": "text/plain"})

    assert response.status == 500
    assert server.wait_for_log(message)


def test_invalid_body_type_after_start_fails_the_response(server: Server) -> None:
    server.mark_log()
    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=10)
    try:
        connection.request("GET", "/bad-body-type")
        with pytest.raises(
            (http.client.IncompleteRead, ConnectionError, http.client.HTTPException)
        ):
            raw = connection.getresponse()
            assert raw.status in (200, 500)
            raw.read()
            raise http.client.HTTPException("the response was completed")
    finally:
        connection.close()

    assert server.wait_for_log("body must be bytes")


def test_trailers_are_refused_as_not_advertised(server: Server) -> None:
    server.mark_log()
    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=10)
    try:
        connection.request("GET", "/trailers")
        with pytest.raises(
            (http.client.IncompleteRead, ConnectionError, http.client.HTTPException)
        ):
            raw = connection.getresponse()
            raw.read()
            raise http.client.HTTPException("the response was completed")
    finally:
        connection.close()

    assert server.wait_for_log("http.response.trailers is not advertised")
