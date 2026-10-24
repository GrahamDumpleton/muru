"""REQUEST_METHOD for each HTTP method, including HEAD."""

from __future__ import annotations

import pytest
from support.server import Server

AREA = "/method"


@pytest.mark.parametrize("method", ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"])
def test_method_is_passed_through(server: Server, method: str) -> None:
    response = server.request(method, f"{AREA}/echo")
    assert response.header("X-Method-Seen") == method
    assert response.body == f"method={method};end".encode()


def test_head_reaches_the_application_as_head(server: Server) -> None:
    response = server.request("HEAD", f"{AREA}/echo")
    assert response.status == 200
    assert response.header("X-Method-Seen") == "HEAD"
    assert response.body == b""


def test_head_response_has_no_body_on_the_wire(server: Server) -> None:
    raw = server.raw(
        b"HEAD /method/echo HTTP/1.1\r\nHost: t\r\nConnection: close\r\nConnection: close\r\n\r\n"
    )
    head, _, body = raw.partition(b"\r\n\r\n")
    assert head.startswith(b"HTTP/1.1 200")
    assert body == b""
