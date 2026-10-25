"""Custom error pages, the error override and maintenance mode (section 4.12).

A page is configured by status or status:reason at four levels, the
route, the release, the host and the node, and is a file, a template
the edge renders, or an application path the edge asks for the page as
a sub-request. The override replaces an application's own error
responses with those pages. ``muructl maintenance on`` serves the
maintenance page for an application at the edge, with exemptions.
"""

from __future__ import annotations

import http.client
import json
import shutil
import sys
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon

STATIC = support.ROOT / "tests" / "fixtures" / "static"
MAINTENANCE_PAGE = b"<!doctype html><h1>Back soon</h1>\n"


def call(
    port: int,
    path: str,
    host: str = "site.test",
    headers: dict[str, str] | None = None,
    method: str = "GET",
) -> support.Response:
    """One request with a Host header."""

    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)

    try:
        connection.request(method, path, headers={"Host": host, **(headers or {})})
        raw = connection.getresponse()
        return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
    finally:
        connection.close()


def shop_copy(base: Path) -> Path:
    """A copy of the shop project with a pages directory of its own."""

    copy = base / "shop"
    shutil.copytree(SHOP, copy, ignore=shutil.ignore_patterns("__pycache__"))
    (copy / "pages").mkdir()
    (copy / "pages" / "maintenance.html").write_bytes(MAINTENANCE_PAGE)
    return copy


@pytest.fixture(scope="module")
def site(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Daemon]:
    """A daemon serving the shop with pages at every level."""

    base = tmp_path_factory.mktemp("errors")
    source = shop_copy(base)
    daemon = start_daemon(base)

    try:
        config = base / "site.toml"
        config.write_text(
            f"""[app.shop.release.r1]
source = '{source}'
python = '{sys.executable}'
processes = 1
error_override = {{ statuses = [401, 404, 500], when = 'html_only' }}

[app.shop.release.r1.error_pages]
"503:maintenance" = {{ file = 'pages/maintenance.html' }}
500 = {{ path = '/error-page' }}
401 = {{ template = 'release {{status}} {{title}}' }}
404 = {{ template = 'release {{status}} {{reason}} {{path}}', content_type = 'text/plain' }}

[app.shop.release.r1.maintenance]
exempt_paths = ['/health']
exempt_addresses = ['10.0.0.0/8']
bypass_cookie = 'muru_bypass=letmein'
retry_after = '2m'

[host.site]
names = ['site.test']
default = true

[host.bare]
names = ['bare.test']

[host.bare.error_pages."404:no_route"]
template = 'no route on {{host}}{{path}} ({{request_id}})'
content_type = 'text/plain'

[[route]]
host = 'site'
path = '/'
app = 'shop'

[[route]]
host = 'site'
path = '/plain/'
app = 'shop'
error_override = {{ statuses = [404], when = 'all' }}

[route.error_pages]
500 = {{ template = '<p>route {{status}} {{reason}} {{path}}</p>' }}
404 = {{ path = '/error-fail' }}

[[route]]
host = 'site'
path = '/api/'
app = 'shop'
error_override = {{ statuses = [404], when = 'not_json' }}

[[route]]
host = 'site'
path = '/slow/'
app = 'shop'
error_pages = {{ 500 = {{ path = '/error-slow' }} }}

[[route]]
host = 'site'
path = '/static/'
static = '{STATIC}'

[[route]]
host = 'bare'
path = '/app/'
app = 'shop'
"""
        )
        applied = daemon.muructl("apply", "-f", str(config), check=False)
        assert applied.returncode == 0, applied.stdout + applied.stderr
        yield daemon
    finally:
        daemon.stop()


def test_a_host_page_answers_what_the_host_refuses(site: Daemon) -> None:
    response = call(site.port, "/missing?x=1", host="bare.test")
    assert response.status == 404
    assert response.header("Content-Type") == "text/plain"
    assert response.header("Cache-Control") == "no-store"
    request_id = response.header("X-Request-ID")
    assert response.body.decode() == f"no route on bare.test/missing ({request_id})"

    # A status:reason page is only for that reason: the host's 404 page
    # does not answer the override's 404s, which the release's does.

    response = call(site.port, "/app/status/404", host="bare.test", headers={"Accept": "text/html"})
    assert response.status == 404
    assert response.body.decode() == "release 404 error_override /app/status/404"


def test_the_override_replaces_the_applications_own_error_page(site: Daemon) -> None:
    # html_only: a browser gets the release's page, an API client keeps
    # the application's document.

    response = call(site.port, "/status/404", headers={"Accept": "text/html"})
    assert response.status == 404
    assert response.header("Content-Type") == "text/plain"
    assert response.body.decode() == "release 404 error_override /status/404"

    response = call(site.port, "/status/404?body=json", headers={"Accept": "application/json"})
    assert response.status == 404
    assert json.loads(response.body)["error"] == "the application's own document"

    # The application opts out for one response, and the header never
    # reaches the client.

    response = call(site.port, "/status/404?override=off", headers={"Accept": "text/html"})
    assert response.status == 404
    assert response.body == b"<h1>the application's own page</h1>\n"
    assert response.header("X-Muru-Error-Override") is None

    # A status the override does not list is untouched.

    response = call(site.port, "/status/403", headers={"Accept": "text/html"})
    assert response.status == 403
    assert response.body == b"<h1>the application's own page</h1>\n"


def test_the_override_keeps_the_headers_a_client_needs(site: Daemon) -> None:
    response = call(site.port, "/status/401?www=1", headers={"Accept": "text/html"})
    assert response.status == 401
    assert response.header("WWW-Authenticate") == 'Basic realm="shop"'
    assert response.body.decode() == "release 401 Unauthorized"
    assert response.header("Content-Type") == "text/html; charset=utf-8"


def test_not_json_keeps_json_documents(site: Daemon) -> None:
    response = call(site.port, "/api/status/404?body=json")
    assert response.status == 404
    assert json.loads(response.body)["error"] == "the application's own document"

    response = call(site.port, "/api/status/404?body=html")
    assert response.status == 404
    assert response.body.decode() == "release 404 error_override /api/status/404"


def test_an_application_path_serves_the_page_with_the_errors_status(site: Daemon) -> None:
    response = call(site.port, "/error")
    assert response.status == 500
    assert response.header("Content-Type") == "application/json"
    assert response.header("X-Handler") == "shop"
    assert response.header("Cache-Control") == "no-store"
    request_id = response.header("X-Request-ID")
    report = json.loads(response.body)
    assert report["internal"] is True
    assert report["redirect_status"] == "500"
    assert report["redirect_url"] == "/error"
    assert report["error_reason"] == "app_error"
    assert report["request_id"] == f"{request_id}-1"
    assert report["method"] == "GET"

    # HEAD stays HEAD, and gets the handler's headers with no body.

    response = call(site.port, "/error", method="HEAD")
    assert response.status == 500
    assert response.header("X-Handler") == "shop"
    assert response.body == b""


def test_a_route_page_wins_over_the_releases(site: Daemon) -> None:
    response = call(site.port, "/plain/error")
    assert response.status == 500
    assert response.header("Content-Type") == "text/html; charset=utf-8"
    assert response.body.decode() == "<p>route 500 app_error /plain/error</p>"


def test_a_failing_handler_leaves_the_built_in_page(site: Daemon) -> None:
    # The route's 404 page is an application path that raises, so the
    # override's page falls back to the built-in one, and never recurses.

    response = call(site.port, "/plain/status/404", headers={"Accept": "application/json"})
    assert response.status == 404
    problem = json.loads(response.body)
    assert problem["reason"] == "error_override"
    assert problem["request_id"] == response.header("X-Request-ID")


def test_a_slow_handler_is_cut_off(site: Daemon) -> None:
    started = time.monotonic()
    response = call(site.port, "/slow/error", headers={"Accept": "text/plain"})
    elapsed = time.monotonic() - started
    assert response.status == 500
    assert response.body.startswith(b"500 Internal Server Error\nreason: app_error\n")
    assert 1.5 < elapsed < 2.9, elapsed


def test_maintenance_mode(site: Daemon) -> None:
    assert "[maintenance]" not in site.muructl("app", "list").stdout
    turned_on = site.muructl("maintenance", "on", "shop").stdout
    assert "shop is in maintenance" in turned_on

    try:
        response = call(site.port, "/")
        assert response.status == 503
        assert response.header("Retry-After") == "120"
        assert response.header("Content-Type") == "text/html; charset=utf-8"
        assert response.header("Cache-Control") == "no-store"
        assert response.body == MAINTENANCE_PAGE

        # Exempt paths, the bypass cookie and static routes are served.

        assert call(site.port, "/health").status == 200
        assert call(site.port, "/", headers={"Cookie": "muru_bypass=letmein"}).status == 200
        assert call(site.port, "/", headers={"Cookie": "muru_bypass=nope"}).status == 503
        assert call(site.port, "/static/fox.txt").status == 200

        # Another host's route to the application is in maintenance too.

        assert call(site.port, "/app/", host="bare.test").status == 503

        listed = site.muructl("app", "list").stdout
        assert "shop" in listed and "[maintenance]" in listed
        assert "maintenance.changed" in site.events()

        # Turning it on again changes nothing.

        assert "shop is in maintenance" in site.muructl("maintenance", "on", "shop").stdout
    finally:
        turned_off = site.muructl("maintenance", "off", "shop").stdout

    assert "shop serves as usual again" in turned_off
    assert call(site.port, "/").status == 200
    assert "[maintenance]" not in site.muructl("app", "list").stdout


def test_maintenance_survives_a_daemon_restart(site: Daemon, tmp_path: Path) -> None:
    site.muructl("maintenance", "on", "shop")

    try:
        site.stop(shutdown=False)
        restarted = start_daemon(site.base, run_dir=site.run_dir, port=site.port)
        site.process = restarted.process
        assert call(site.port, "/").status == 503
        assert call(site.port, "/health").status == 200
    finally:
        site.muructl("maintenance", "off", "shop")

    assert call(site.port, "/").status == 200


def test_apply_refuses_bad_tables(site: Daemon, tmp_path: Path) -> None:
    def refused(table: str) -> str:
        config = tmp_path / "bad.toml"
        config.write_text(
            f"""[host.extra]
names = ['extra.test']
{table}
"""
        )
        completed = site.muructl("apply", "-f", str(config), check=False)
        assert completed.returncode != 0, completed.stdout
        return completed.stdout + completed.stderr

    assert "when must be all, empty_body, not_json or html_only" in refused(
        "error_override = { statuses = [404], when = 'sometimes' }"
    )
    assert "is not a status from 400 to 599" in refused(
        "error_pages = { abc = { template = 'x' } }"
    )
    assert "exactly one of file, template and path" in refused(
        "error_pages = { 404 = { template = 'x', path = '/x' } }"
    )
    assert "file must be an absolute path here" in refused(
        "error_pages = { 404 = { file = 'pages/404.html' } }"
    )


def test_development_mode_takes_pages_from_the_project(tmp_path: Path) -> None:
    source = shop_copy(tmp_path)
    (source / "pages" / "500.html").write_text("<h1>Our fault</h1>\n")
    (source / "pyproject.toml").write_text(
        """[project]
name = "shop"
version = "0.0.0"

[tool.muru]
entrypoint = "shop.wsgi:application"

[tool.muru.error_pages]
500 = { file = "pages/500.html" }
"503:maintenance" = { file = "pages/maintenance.html" }
"""
    )
    server = support.start(str(source), workers=1, threads=2, chdir=None)

    try:
        response = server.get("/error", headers={"Accept": "application/json"})
        assert response.status == 500
        assert response.header("Content-Type") == "text/html; charset=utf-8"
        assert response.body == b"<h1>Our fault</h1>\n"
        assert response.header("Cache-Control") == "no-store"
    finally:
        server.stop()
