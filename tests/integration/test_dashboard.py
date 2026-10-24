"""The read-only dashboard of muructl run (design specification section 10.7)."""

from __future__ import annotations

import http.client
from collections.abc import Iterator

import pytest
from support import server as support


def fetch(port: int, path: str) -> tuple[int, dict[str, str], bytes]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)

    try:
        connection.request("GET", path)
        response = connection.getresponse()
        return response.status, {k.lower(): v for k, v in response.getheaders()}, response.read()
    finally:
        connection.close()


@pytest.fixture(scope="module")
def dashboard() -> Iterator[tuple[support.Server, int]]:
    port = support.free_port()
    instance = support.start(
        "wsgi_conformance:application", workers=2, args=["--dashboard", f"127.0.0.1:{port}"]
    )

    try:
        assert instance.wait_for_log(f"dashboard on http://127.0.0.1:{port}/")
        yield instance, port
    finally:
        instance.stop()


def test_page_shows_workers_requests_and_logs(dashboard: tuple[support.Server, int]) -> None:
    server, port = dashboard
    assert server.get("/response/list").status == 200
    assert server.get("/response/raise-first").status == 500
    status, headers, body = fetch(port, "/")
    assert status == 200
    assert headers["content-type"].startswith("text/html")
    assert "default-src 'none'" in headers["content-security-policy"]
    page = body.decode()
    assert "<title>Muru: wsgi_conformance:application</title>" in page
    assert "release r1" in page
    assert "Workers" in page and page.count("worker-") >= 2
    assert "/response/list" in page
    assert "/response/raise-first" in page and "app_error" in page
    assert "muru.host[" in page and "preloaded" in page
    assert "raise-first-boom" in page


def test_fragments_and_assets(dashboard: tuple[support.Server, int]) -> None:
    _server, port = dashboard

    for path in ("/fragment/overview", "/fragment/requests", "/fragment/logs"):
        status, headers, body = fetch(port, path)
        assert status == 200, path
        assert headers["content-type"].startswith("text/html")
        assert body

    status, headers, body = fetch(port, "/static/htmx.min.js")
    assert status == 200
    assert headers["content-type"] == "text/javascript"
    assert body.startswith(b"var htmx=")
    assert fetch(port, "/missing")[0] == 404


def test_dashboard_can_be_off() -> None:
    server = support.start("plain_wsgi:application", workers=1, args=["--dashboard", "off"])

    try:
        assert server.get("/").status == 200
        assert "dashboard" not in server.log()
    finally:
        server.stop()


def test_dashboard_refuses_non_loopback_addresses() -> None:
    server = support.start("plain_wsgi:application", workers=1, args=["--dashboard", "0.0.0.0:0"])

    try:
        assert server.wait_for_log("no dashboard: the dashboard is only served on loopback")
        assert server.get("/").status == 200
    finally:
        server.stop()
