"""start_response: the write callable, exc_info, repeated calls and validation."""

from __future__ import annotations

import pytest
from support.server import Server, text

AREA = "/start-response"


def test_write_callable_output_precedes_the_iterable(server: Server) -> None:
    response = server.get(f"{AREA}/write")
    assert response.status == 200
    assert response.body == b"via-write-via-iter"


def test_write_calls_interleave_with_the_iterable(server: Server) -> None:
    assert server.get(f"{AREA}/write-before-return").body == b"pre-mid-post1-post2"


def test_exc_info_must_be_a_tuple_or_none(server: Server) -> None:
    body = text(server.get(f"{AREA}/non-tuple-exc-info"))
    assert body.startswith("TypeError:")
    assert "exc_info" in body


def test_exc_info_after_headers_sent_reraises_the_exception(server: Server) -> None:
    response = server.get(f"{AREA}/exc-info-after-headers")
    assert response.status == 200
    assert response.body == b"headers-sent-reraised:intentional-exc-info"


def test_exc_info_before_headers_sent_replaces_status_and_headers(server: Server) -> None:
    response = server.get(f"{AREA}/exc-info-before-headers")
    assert response.status == 418
    assert response.body == b"second-headers-win"
    assert response.header("X-After") == "second"
    assert response.header("X-Before") is None


def test_second_call_before_output_replaces_status_and_headers(server: Server) -> None:
    # PEP 3333 forbids a second call without exc_info; like mod_wsgi, Muru
    # tolerates it while nothing has reached the wire and the second call
    # wins. The strictness applies once output has started.

    response = server.get(f"{AREA}/double-before-output")
    assert response.status == 201
    assert response.header("X-First") is None
    assert response.header("X-Second") == "2"


def test_second_call_after_output_raises(server: Server) -> None:
    response = server.get(f"{AREA}/double-after-output")
    assert response.status == 200
    assert response.body.startswith(b"pre-err:")


@pytest.mark.parametrize(
    "case",
    ["no-digits", "mixed", "four-digits", "no-space", "control-char", "empty"],
)
def test_invalid_status_is_a_value_error(server: Server, case: str) -> None:
    body = text(server.get(f"{AREA}/invalid-status?case={case}"))
    assert body.startswith("ValueError:"), body


def test_non_string_status_is_a_type_error(server: Server) -> None:
    body = text(server.get(f"{AREA}/invalid-status?case=not-str"))
    assert body.startswith("TypeError:"), body


@pytest.mark.parametrize(
    ("case", "error"),
    [
        ("empty-name", "ValueError"),
        ("space-name", "ValueError"),
        ("control-name", "ValueError"),
        ("cr-value", "ValueError"),
        ("lf-value", "ValueError"),
        ("non-string-name", "TypeError"),
        ("non-string-value", "TypeError"),
        ("non-latin1-value", "UnicodeEncodeError"),
        ("not-a-tuple", "TypeError"),
        ("wrong-tuple-size", "ValueError"),
        ("bad-content-length", "ValueError"),
    ],
)
def test_invalid_header_is_rejected(server: Server, case: str, error: str) -> None:
    body = text(server.get(f"{AREA}/invalid-header?case={case}"))
    assert body.startswith(f"{error}:"), body


@pytest.mark.parametrize("case", ["string", "tuple", "dict"])
def test_headers_must_be_a_list(server: Server, case: str) -> None:
    body = text(server.get(f"{AREA}/invalid-headers-type?case={case}"))
    assert body.startswith("TypeError:"), body


def test_output_without_start_response_is_a_500(server: Server) -> None:
    server.mark_log()
    response = server.get(f"{AREA}/never-called")
    assert response.status == 500
    assert server.wait_for_log("start_response")


def test_reason_phrase_reaches_the_client(server: Server) -> None:
    response = server.get(f"{AREA}/status-reason-phrase")
    assert response.status == 299
    assert response.reason == "Custom Reason Here"
