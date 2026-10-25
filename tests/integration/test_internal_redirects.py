"""Internal redirects (design specification section 4.10).

An application answers with ``X-Muru-Redirect`` and the edge routes the
request afresh: to an internal static route, which clients cannot reach
directly, with the response headers the application chose carried over
and the client's own headers kept, so ranges work against the file; or
to a path of the same application, pinned to the release that
redirected, which sees ``REDIRECT_*`` and ``muru.is_internal()``; or to
another application, only where its route accepts the redirect. The
chain is bounded, the other redirect forms are opt-in, and one access
log entry records every hop.
"""

from __future__ import annotations

import http.client
import json
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon

STATIC = support.ROOT / "tests" / "fixtures" / "static"


def call(
    port: int,
    method: str,
    path: str,
    headers: dict[str, str] | None = None,
    body: bytes | None = None,
    chunked: bool = False,
) -> support.Response:
    """One request of any method, with a Host header for the site.

    With `chunked` the body goes without a Content-Length, which the
    edge buffers before dispatch.
    """

    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    sent = iter([body]) if chunked and body is not None else body

    try:
        connection.request(
            method,
            path,
            body=sent,
            headers={"Host": "site.test", **(headers or {})},
            encode_chunked=chunked,
        )
        raw = connection.getresponse()
        return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
    finally:
        connection.close()


@pytest.fixture(scope="module")
def site(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Daemon]:
    """A daemon serving the shop, a second copy of it as ``other``, and the routes."""

    base = tmp_path_factory.mktemp("redirects")
    daemon = start_daemon(base)

    try:
        assert daemon.deploy("other", SHOP, "--processes", "1").endswith("is ready and serving")
        config = base / "site.toml"
        config.write_text(
            f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1
accel_redirect = true
cgi_local_redirect = true

[host.site]
names = ['site.test']
default = true

[[route]]
host = 'site'
path = '/'
app = 'shop'

[[route]]
host = 'site'
path = '/protected/'
static = '{STATIC}'
internal = true
accept_redirects_from = ['shop']

[[route]]
host = 'site'
path = '/other/'
app = 'other'

[[route]]
host = 'site'
path = '/other-open/'
app = 'other'
accept_redirects_from = ['shop']
"""
        )
        applied = daemon.muructl("apply", "-f", str(config), check=False)
        assert applied.returncode == 0, applied.stdout + applied.stderr
        yield daemon
    finally:
        daemon.stop()


def test_a_protected_file_is_served_through_the_application(site: Daemon) -> None:
    response = call(site.port, "GET", "/download?file=fox.txt")
    assert response.status == 200, response.body
    assert response.body == (STATIC / "fox.txt").read_bytes()

    # The file's type, with the disposition and cache headers the
    # application chose.

    assert (response.header("Content-Type") or "").startswith("text/plain")
    assert response.header("Content-Disposition") == 'attachment; filename="fox.txt"'
    assert response.header("Cache-Control") == "private"
    assert response.header("X-Muru-Redirect") is None

    # The client's own headers reach the file: a range, and HEAD stays HEAD.

    partial = call(site.port, "GET", "/download?file=fox.txt", {"Range": "bytes=0-2"})
    assert partial.status == 206
    assert partial.body == (STATIC / "fox.txt").read_bytes()[:3]
    head = call(site.port, "HEAD", "/download?file=fox.txt")
    assert head.status == 200 and head.body == b""
    assert head.header("Content-Length") == str((STATIC / "fox.txt").stat().st_size)


def test_internal_routes_answer_404_to_clients(site: Daemon) -> None:
    direct = call(site.port, "GET", "/protected/fox.txt", {"Accept": "application/json"})
    assert direct.status == 404
    assert json.loads(direct.body)["reason"] == "internal_only"
    listed = site.muructl("route", "list").stdout
    assert "[internal]" in listed and "[redirects from shop]" in listed


def test_a_redirect_within_the_application_carries_the_context(site: Daemon) -> None:
    response = call(
        site.port,
        "POST",
        "/redirect-to?path=/internal-info&header=X-User:alice&status=202",
        {"X-User": "mallory", "X-Muru-Secret": "forged", "Content-Type": "text/plain"},
        body=b"the body",
    )
    assert response.status == 200, response.body
    report = json.loads(response.body)
    assert report["internal"] is True
    assert report["app"] == "shop"
    assert report["redirect_url"] == "/redirect-to"
    assert report["redirect_query_string"] == "path=/internal-info&header=X-User:alice&status=202"
    assert report["redirect_status"] == "202"

    # The sub-request is a GET without the body, the trusted header
    # replaces the client's, and a client's X-Muru header never arrives.

    assert report["method"] == "GET" and report["body"] == ""
    assert report["x_user"] == "alice"
    assert report["x_muru_secret"] is None
    assert report["request_id"].endswith("-1")

    # A client's own request is not internal.

    plain = json.loads(call(site.port, "GET", "/internal-info").body)
    assert plain["internal"] is False and plain["redirect_url"] is None


def test_the_method_and_body_can_be_preserved_when_the_edge_buffered_it(site: Daemon) -> None:
    # A body without a declared length is buffered before dispatch, so
    # the sub-request can carry it again.

    response = call(
        site.port,
        "POST",
        "/redirect-to?path=/internal-info&method=preserve",
        {"Content-Type": "text/plain"},
        body=b"kept",
        chunked=True,
    )
    assert response.status == 200, response.body
    report = json.loads(response.body)
    assert report["method"] == "POST" and report["body"] == "kept"

    # A body with a declared length streams to the application as it
    # arrives, so there is nothing to send again: the sub-request is a GET.

    streamed = call(
        site.port,
        "POST",
        "/redirect-to?path=/internal-info&method=preserve",
        {"Content-Type": "text/plain"},
        body=b"gone",
    )
    report = json.loads(streamed.body)
    assert report["method"] == "GET" and report["body"] == ""


def test_another_application_takes_redirects_only_where_its_route_accepts_them(
    site: Daemon,
) -> None:
    refused = call(
        site.port, "GET", "/redirect-to?path=/other/internal-info", {"Accept": "application/json"}
    )
    assert refused.status == 404
    assert json.loads(refused.body)["reason"] == "no_route"

    accepted = call(site.port, "GET", "/redirect-to?path=/other-open/internal-info")
    assert accepted.status == 200, accepted.body
    report = json.loads(accepted.body)
    assert report["app"] == "other" and report["internal"] is True


def test_a_redirect_loop_is_cut_off(site: Daemon) -> None:
    response = call(site.port, "GET", "/loop", {"Accept": "application/json"})
    assert response.status == 500
    assert json.loads(response.body)["reason"] == "redirect_loop"


def test_accel_and_cgi_redirects_are_opt_in(site: Daemon) -> None:
    # The shop's release opted in to both forms.

    accel = json.loads(call(site.port, "GET", "/accel?path=/internal-info").body)
    assert accel["internal"] is True and accel["redirect_url"] == "/accel"
    cgi = json.loads(call(site.port, "GET", "/cgi?path=/internal-info").body)
    assert cgi["internal"] is True and cgi["redirect_url"] == "/cgi"

    # The other application did not: its headers pass through.

    passed = call(site.port, "GET", "/other/accel?path=/other/internal-info")
    assert passed.status == 200
    assert passed.header("X-Accel-Redirect") == "/other/internal-info"
    assert passed.body == b""
    location = call(site.port, "GET", "/other/cgi?path=/other/internal-info")
    assert location.status == 200 and location.header("Location") == "/other/internal-info"


def test_accept_redirects_from_must_name_an_application(site: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "bad.toml"
    config.write_text(
        "[[route]]\nhost = 'site'\npath = '/files/'\n"
        f"static = '{STATIC}'\ninternal = true\naccept_redirects_from = ['nobody']\n"
    )
    refused = site.muructl("apply", "-f", str(config), check=False)
    assert refused.returncode == 1
    assert (
        "accept_redirects_from names nobody, which is neither an application nor an upstream"
        in (refused.stdout + refused.stderr)
    )


def test_an_asgi_application_redirects_in_development_mode() -> None:
    server = support.start("plain_asgi:application", workers=1, threads=None)

    try:
        server.mark_log()
        response = server.get("/redirect-to?path=/info&header=X-User:alice")
        assert response.status == 200, response.body
        report = json.loads(response.body)
        muru = report["extensions"]["muru"]
        assert muru["internal"] is True
        assert muru["redirect_url"] == "/redirect-to"
        assert muru["redirect_query_string"] == "path=/info&header=X-User:alice"
        assert muru["redirect_status"] == 200
        assert ["x-user", "alice"] in report["scope"]["headers"]
        assert report["scope"]["path"] == "/info"
        assert "internal" not in json.loads(server.get("/info").body)["extensions"]["muru"]

        # One access log line for the chain, naming what the client asked
        # for and where it went.

        assert server.wait_for_log("redirects=/info")
        line = next(line for line in server.log_since_mark().splitlines() if "redirects=" in line)
        assert "GET /redirect-to?path=/info&header=X-User:alice" in line
    finally:
        server.stop()
