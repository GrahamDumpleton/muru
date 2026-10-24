"""The environ dictionary: CGI variables, wsgi.* keys and native strings."""

from __future__ import annotations

import pytest
from support.server import Server, text

AREA = "/environ"


def value(server: Server, key: str, headers: dict[str, str] | None = None) -> str:
    return text(server.get(f"{AREA}/get?key={key}", headers=headers)).removeprefix("value=")


def present(server: Server, key: str, headers: dict[str, str] | None = None) -> bool:
    return text(server.get(f"{AREA}/has?key={key}", headers=headers)) == "present=YES"


def kind(server: Server, key: str, headers: dict[str, str] | None = None) -> str:
    return text(server.get(f"{AREA}/type?key={key}", headers=headers)).removeprefix("type=")


@pytest.mark.parametrize(
    ("key", "expected"),
    [
        ("REQUEST_METHOD", "GET"),
        ("SERVER_PROTOCOL", "HTTP/1.1"),
        ("SCRIPT_NAME", ""),
        ("PATH_INFO", "/environ/get"),
        ("QUERY_STRING", "key=QUERY_STRING"),
        ("wsgi.url_scheme", "http"),
        ("wsgi.multithread", "True"),
        ("wsgi.multiprocess", "True"),
        ("wsgi.run_once", "False"),
        ("wsgi.input_terminated", "True"),
        ("wsgi.version", "(1, 0)"),
        ("REMOTE_ADDR", "127.0.0.1"),
        ("SERVER_NAME", "127.0.0.1"),
    ],
)
def test_values(server: Server, key: str, expected: str) -> None:
    assert value(server, key) == expected


def test_server_port_is_the_listening_port(server: Server) -> None:
    assert value(server, "SERVER_PORT") == str(server.port)


def test_http_host_is_the_host_header(server: Server) -> None:
    assert value(server, "HTTP_HOST", {"Host": "example.test:8080"}) == "example.test:8080"


def test_content_type_and_length_use_bare_keys(server: Server) -> None:
    headers = {"Content-Type": "application/x-custom"}
    response = server.post(f"{AREA}/get?key=CONTENT_TYPE", b"payload", headers)
    assert text(response) == "value=application/x-custom"
    response = server.post(f"{AREA}/get?key=CONTENT_LENGTH", b"payload", headers)
    assert text(response) == "value=7"
    for key in ("HTTP_CONTENT_TYPE", "HTTP_CONTENT_LENGTH"):
        response = server.post(f"{AREA}/has?key={key}", b"payload", headers)
        assert text(response) == "present=NO"


def test_content_keys_absent_without_a_body(server: Server) -> None:
    assert not present(server, "CONTENT_TYPE")
    assert not present(server, "CONTENT_LENGTH")


def test_custom_headers_become_http_keys(server: Server) -> None:
    assert value(server, "HTTP_X_REQUEST_ID", {"X-Request-Id": "req-abc-123"}) == "req-abc-123"
    assert not present(server, "HTTP_X_REQUEST_ID")
    assert value(server, "HTTP_X_WITH_DASHES", {"X-With-Dashes": "dash-test"}) == "dash-test"


def test_repeated_headers_are_joined(server: Server) -> None:
    request = (
        b"GET /environ/get?key=HTTP_X_MULTI HTTP/1.1\r\nHost: t\r\nConnection: close\r\n"
        b"X-Multi: a\r\nX-Multi: b\r\n\r\n"
    )
    assert b"value=a, b;end" in server.raw(request)


def test_underscore_headers_are_dropped(server: Server) -> None:
    # Section 4.7: X_Forwarded_For must not reach HTTP_X_FORWARDED_FOR.
    request = (
        b"GET /environ/has?key=HTTP_X_FORWARDED_FOR HTTP/1.1\r\nHost: t\r\n"
        b"Connection: close\r\nX_Forwarded_For: 1.2.3.4\r\n\r\n"
    )
    assert b"present=NO;end" in server.raw(request)


@pytest.mark.parametrize("key", ["PATH", "HTTPS", "HTTP_CONTENT_TYPE"])
def test_absent_keys(server: Server, key: str) -> None:
    assert not present(server, key)


@pytest.mark.parametrize(
    "key",
    [
        "REQUEST_METHOD",
        "PATH_INFO",
        "SCRIPT_NAME",
        "QUERY_STRING",
        "SERVER_PROTOCOL",
        "SERVER_NAME",
        "SERVER_PORT",
        "REMOTE_ADDR",
        "wsgi.url_scheme",
    ],
)
def test_cgi_values_are_native_strings(server: Server, key: str) -> None:
    assert kind(server, key) == "builtins.str"


def test_custom_header_values_are_native_strings(server: Server) -> None:
    assert kind(server, "HTTP_X_CUSTOM_STR", {"X-Custom-Str": "hello"}) == "builtins.str"


@pytest.mark.parametrize(
    ("key", "expected"),
    [
        ("wsgi.input", "muru._native.Input"),
        ("wsgi.errors", "_io.TextIOWrapper"),
        ("wsgi.file_wrapper", "muru._native.FileWrapper"),
        ("wsgi.multiprocess", "builtins.bool"),
        ("wsgi.version", "builtins.tuple"),
    ],
)
def test_wsgi_object_types(server: Server, key: str, expected: str) -> None:
    assert kind(server, key) == expected


def test_muru_keys(server: Server) -> None:
    assert present(server, "muru.request_id")
    assert kind(server, "muru.request_id") == "builtins.str"


def test_all_http_keys_listed(server: Server) -> None:
    body = text(server.get(f"{AREA}/all-http", {"X-One": "1", "X-Two": "2"}))
    assert "HTTP_X_ONE" in body
    assert "HTTP_X_TWO" in body
    assert "HTTP_HOST" in body


def test_path_info_is_percent_decoded_as_latin1(server: Server) -> None:
    response = server.get(f"{AREA}/path-bytes/caf%C3%A9%20x")
    assert text(response) == "path=/environ/path-bytes/caf\xe9 x"


def test_header_values_are_decoded_as_latin1(server: Server) -> None:
    request = (
        b"GET /environ/get?key=HTTP_X_LATIN HTTP/1.1\r\nHost: t\r\nConnection: close\r\n"
        b"X-Latin: caf\xe9\r\n\r\n"
    )
    assert b"value=caf\xc3\xa9;end" in server.raw(request)
