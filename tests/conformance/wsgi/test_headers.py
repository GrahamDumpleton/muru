"""Response headers given to start_response reach the client intact."""

from __future__ import annotations

from support.server import Server

AREA = "/headers"


def test_custom_header(server: Server) -> None:
    response = server.get(f"{AREA}/custom")
    assert response.status == 200
    assert response.header("X-Test-Custom") == "hello-world"


def test_multiple_headers(server: Server) -> None:
    response = server.get(f"{AREA}/multiple")
    assert response.header("X-Test-First") == "one"
    assert response.header("X-Test-Second") == "two"
    assert response.header("X-Test-Third") == "three"


def test_repeated_header_keeps_both_values(server: Server) -> None:
    response = server.get(f"{AREA}/repeated")
    assert response.header_values("X-Test-Repeated") == ["first-value", "second-value"]


def test_set_cookie_headers_stay_separate(server: Server) -> None:
    response = server.get(f"{AREA}/set-cookie")
    assert response.header_values("Set-Cookie") == ["a=1; Path=/", "b=2; Path=/"]


def test_www_authenticate_on_a_401(server: Server) -> None:
    response = server.get(f"{AREA}/www-authenticate")
    assert response.status == 401
    assert response.header("WWW-Authenticate") == 'Basic realm="test"'


def test_many_headers(server: Server) -> None:
    response = server.get(f"{AREA}/many")
    assert response.header("X-Many-01") == "value-1"
    assert response.header("X-Many-13") == "value-13"
    assert response.header("X-Many-25") == "value-25"


def test_latin1_header_value(server: Server) -> None:
    raw = server.raw(b"GET /headers/latin1-value HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
    assert b"x-latin1: caf\xe9\r\n" in raw.lower()


def test_header_names_are_case_insensitive(server: Server) -> None:
    response = server.get(f"{AREA}/mixed-case")
    assert response.header("Content-Type") == "text/plain"
    assert response.header("X-Mixed") == "value"


def test_server_header_is_added(server: Server) -> None:
    assert server.get(f"{AREA}/custom").header("Server") == "muru"
