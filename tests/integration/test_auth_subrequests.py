"""Authentication sub-requests (section 4.16).

A route's ``auth`` table names an application path the edge asks before
it serves the route: a bodiless GET with the client's headers. 2xx
allows the request and the listed headers of the answer travel on to
the target; 401, 403 and redirects go back to the client as the handler
answered; anything else is 503 ``auth_unavailable``. The check guards
static directories and WebSocket upgrades as well as applications, and
allowing decisions can be cached.
"""

from __future__ import annotations

import http.client
import json
import sys
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon

STATIC = support.ROOT / "tests" / "fixtures" / "static"
MOUNTED = support.ROOT / "tests" / "fixtures" / "projects" / "mounted"


def call(
    port: int,
    path: str,
    headers: dict[str, str] | None = None,
    method: str = "GET",
    body: bytes | None = None,
) -> support.Response:
    """One request with a Host header."""

    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)

    try:
        connection.request(
            method, path, body=body, headers={"Host": "site.test", **(headers or {})}
        )
        raw = connection.getresponse()
        return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
    finally:
        connection.close()


def session(name: str) -> dict[str, str]:
    return {"Cookie": f"session={name}"}


@pytest.fixture(scope="module")
def site(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Daemon]:
    """A daemon with the shop as the gatekeeper of several routes."""

    base = tmp_path_factory.mktemp("auth")
    daemon = start_daemon(base)

    try:
        config = base / "site.toml"
        config.write_text(
            f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1

[app.mounted.release.r1]
source = '{MOUNTED}'
python = '{sys.executable}'
processes = 1

[host.site]
names = ['site.test']
default = true

[host.site.error_pages."403:auth_denied"]
template = 'no entry for {{path}}'
content_type = 'text/plain'

[[route]]
host = 'site'
path = '/'
app = 'shop'

[[route]]
host = 'site'
path = '/members/'
app = 'shop'

[route.auth]
app = 'shop'
path = '/auth-check'
copy_headers = ['X-Auth-User', 'X-Auth-Groups', 'X-Auth-Seen']

[[route]]
host = 'site'
path = '/files/'
static = '{STATIC}'
auth = {{ app = 'shop', path = '/auth-check' }}

[[route]]
host = 'site'
path = '/asgi/'
app = 'mounted'
auth = {{ app = 'shop', path = '/auth-check', timeout = '1s' }}

[[route]]
host = 'site'
path = '/by-asgi/'
app = 'shop'
auth = {{ app = 'mounted', path = '/asgi/check', copy_headers = ['X-Auth-Seen'] }}

[[route]]
host = 'site'
path = '/cached/'
app = 'shop'

[route.auth]
app = 'shop'
path = '/auth-check'
copy_headers = ['X-Auth-User']
cache = {{ ttl = '2s', key = ['cookie:session'] }}
"""
        )
        applied = daemon.muructl("apply", "-f", str(config), check=False)
        assert applied.returncode == 0, applied.stdout + applied.stderr
        yield daemon
    finally:
        daemon.stop()


def test_an_allowed_request_reaches_the_target_with_the_handlers_headers(site: Daemon) -> None:
    response = call(
        site.port, "/members/internal-info?q=1", {**session("ok"), "X-Auth-User": "mallory"}
    )
    assert response.status == 200, response.body
    report = json.loads(response.body)
    assert report["internal"] is False, "the request itself is the client's"
    assert report["auth_headers"]["HTTP_X_AUTH_USER"] == "alice"
    assert report["auth_headers"]["HTTP_X_AUTH_GROUPS"] == "staff"
    assert "HTTP_X_AUTH_SECRET" not in report["auth_headers"], "only listed headers travel"

    # What the handler saw: an internal GET with no body, the original
    # request described in the environ and in headers, and no REDIRECT_*.

    seen = json.loads(report["auth_headers"]["HTTP_X_AUTH_SEEN"])
    assert seen["auth_request"] is True and seen["internal"] is True
    assert seen["method"] == "GET"
    assert seen["original_method"] == "GET"
    assert seen["original_uri"] == "/members/internal-info?q=1"
    assert seen["original_host"] == "site.test"
    assert seen["x_original_uri"] == "/members/internal-info?q=1"
    assert seen["redirect_url"] is None
    assert seen["body"] == ""
    assert seen["request_id"] == response.header("X-Request-ID") + "-1"

    # The log line is the client's request, with the handler as a hop.

    time.sleep(0.2)
    access = site.muructl("logs", "--access", "--json").stdout
    records = [json.loads(line) for line in access.splitlines() if line.startswith("{")]
    lines = [record for record in records if record["target"] == "/members/internal-info?q=1"]
    assert len(lines) == 1 and lines[0]["redirects"] == ["/auth-check"], access


def test_the_body_waits_for_the_decision(site: Daemon) -> None:
    response = call(
        site.port,
        "/members/internal-info",
        {**session("ok"), "Content-Type": "text/plain"},
        method="POST",
        body=b"the payload",
    )
    report = json.loads(response.body)
    assert report["method"] == "POST" and report["body"] == "the payload"
    seen = json.loads(report["auth_headers"]["HTTP_X_AUTH_SEEN"])
    assert seen["original_method"] == "POST"
    assert seen["content_length"] in (None, ""), "the handler gets no body"


def test_401_and_redirects_are_the_handlers_own_answers(site: Daemon) -> None:
    response = call(site.port, "/members/internal-info")
    assert response.status == 401
    assert response.header("WWW-Authenticate") == 'Bearer realm="shop"'
    assert response.body == b"who are you\n"
    assert response.header("X-Request-ID")

    response = call(site.port, "/members/internal-info", session("expired"))
    assert response.status == 302
    assert response.header("Location") == "/login?next=/"


def test_a_page_configured_for_the_reason_replaces_a_refusal(site: Daemon) -> None:
    response = call(site.port, "/members/internal-info", session("banned"))
    assert response.status == 403
    assert response.body.decode() == "no entry for /members/internal-info"
    assert response.header("Set-Cookie") == "notice=banned; Path=/", "the handler's cookie is kept"


def test_the_check_fails_closed(site: Daemon) -> None:
    for name in ("broken", "teapot"):
        response = call(
            site.port, "/members/internal-info", {**session(name), "Accept": "application/json"}
        )
        assert response.status == 503, name
        assert json.loads(response.body)["reason"] == "auth_unavailable"

    started = time.monotonic()
    response = call(site.port, "/asgi/info", {**session("slow"), "Accept": "application/json"})
    elapsed = time.monotonic() - started
    assert response.status == 503
    assert json.loads(response.body)["reason"] == "auth_unavailable"
    assert 0.8 < elapsed < 2.5, elapsed


def test_a_static_directory_is_guarded(site: Daemon) -> None:
    assert call(site.port, "/files/fox.txt").status == 401
    response = call(site.port, "/files/fox.txt", session("ok"))
    assert response.status == 200
    assert response.body == (STATIC / "fox.txt").read_bytes()


def test_a_websocket_upgrade_is_checked_first(site: Daemon) -> None:
    websockets = pytest.importorskip("websockets.sync.client")
    from websockets.exceptions import InvalidStatus

    with websockets.connect(
        f"ws://127.0.0.1:{site.port}/asgi/ws",
        additional_headers={"Host": "site.test", "Cookie": "session=ok"},
    ) as socket:
        socket.send("hello")
        assert socket.recv() == "echo:hello"

    with pytest.raises(InvalidStatus) as refused:
        websockets.connect(
            f"ws://127.0.0.1:{site.port}/asgi/ws",
            additional_headers={"Host": "site.test"},
        )

    assert refused.value.response.status_code == 401


def test_an_asgi_handler_sees_the_original_request(site: Daemon) -> None:
    response = call(site.port, "/by-asgi/internal-info?x=1")
    assert response.status == 200
    report = json.loads(response.body)
    assert report["auth_headers"]["HTTP_X_AUTH_SEEN"] == "GET /by-asgi/internal-info?x=1 site.test"


def test_allowing_decisions_are_cached_by_the_key(site: Daemon) -> None:
    def served(cookie: str) -> dict[str, str]:
        response = call(site.port, "/cached/internal-info", {"Cookie": cookie})
        assert response.status == 200, response.body
        return json.loads(response.body)["auth_headers"]

    before = site.muructl("metrics").stdout
    assert served("session=ok")["HTTP_X_AUTH_USER"] == "alice"
    assert served("session=ok")["HTTP_X_AUTH_USER"] == "alice"
    assert served("session=ok; other=1")["HTTP_X_AUTH_USER"] == "alice"
    after = site.muructl("metrics").stdout

    def count(text: str, result: str) -> int:
        for line in text.splitlines():
            if line.startswith(f'muru_auth_requests_total{{result="{result}"}}'):
                return int(float(line.rsplit(" ", 1)[1]))

        return 0

    assert count(after, "allowed") - count(before, "allowed") == 1
    assert count(after, "cached") - count(before, "cached") == 2

    # A refusal is never cached, and an anonymous request is never served
    # from the cache.

    assert call(site.port, "/cached/internal-info", session("banned")).status == 403
    assert call(site.port, "/cached/internal-info").status == 401
    time.sleep(2.1)
    served("session=ok")
    assert count(site.muructl("metrics").stdout, "allowed") - count(before, "allowed") == 2


def test_route_list_and_apply_show_and_check_the_table(site: Daemon, tmp_path: Path) -> None:
    listed = site.muructl("route", "list").stdout
    assert "[auth by shop /auth-check]" in listed

    def refused(table: str) -> str:
        config = tmp_path / "bad.toml"
        config.write_text(
            f"""[host.extra]
names = ['extra.test']

[[route]]
host = 'extra'
path = '/'
app = 'shop'
{table}
"""
        )
        completed = site.muructl("apply", "-f", str(config), check=False)
        assert completed.returncode != 0, completed.stdout
        return completed.stdout + completed.stderr

    assert "auth.app names nobody, which is not an application" in refused(
        "auth = { app = 'nobody', path = '/check' }"
    )
    assert "must start with /" in refused("auth = { app = 'shop', path = 'check' }")
    assert "cookie:NAME or header:NAME" in refused(
        "auth = { app = 'shop', path = '/check', cache = { ttl = '1s', key = ['query:x'] } }"
    )
    assert "starts with X-Muru-" in refused(
        "auth = { app = 'shop', path = '/check', copy_headers = ['X-Muru-Redirect'] }"
    )
