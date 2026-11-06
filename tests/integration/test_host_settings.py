"""Settings a virtual host carries for its routes (design specification
sections 4.3, 4.6 and 11.2).

A host sets the limits of a request, which a route of the host replaces
key by key and the daemon's `[limits]` table supplies where neither says;
redirects its other names to its canonical name; and chooses which of
its requests the access log writes.
"""

from __future__ import annotations

import gzip
import http.client
import json
import subprocess
import sys
import time
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon

APPS = support.ROOT / "tests" / "fixtures" / "apps"

ECHO = f"""[app.echo.release.r1]
source = '{APPS}'
python = '{sys.executable}'
entrypoint = 'plain_wsgi:application'

"""


@pytest.fixture
def daemon(tmp_path: Path) -> Iterator[Daemon]:
    started = start_daemon(tmp_path)

    try:
        yield started
    finally:
        started.stop()


def wait_for(predicate: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if predicate():
            return

        time.sleep(0.05)

    raise AssertionError("the condition did not hold in time")


def apply(daemon: Daemon, path: Path) -> subprocess.CompletedProcess[str]:
    return daemon.muructl("apply", "-f", str(path), check=False)


def post(daemon: Daemon, path: str, host: str, size: int) -> int:
    """The status of a POST of `size` bytes."""

    connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=30)

    try:
        connection.request("POST", path, body=b"x" * size, headers={"Host": host})
        response = connection.getresponse()
        response.read()
        return response.status
    finally:
        connection.close()


def test_a_hosts_limits_apply_to_its_routes_and_a_route_replaces_them(
    daemon: Daemon, tmp_path: Path
) -> None:
    config = tmp_path / "hosts.toml"
    config.write_text(
        ECHO
        + """[host.api]
names = ["api.test"]
max_body_size = "1KB"
timeouts = { request_deadline = "1s", queue_timeout = "2s" }

[host.other]
names = ["other.test"]

[[route]]
host = "api"
app = "echo"

[[route]]
host = "api"
path = "/echo"
app = "echo"
max_body_size = "1MB"

[[route]]
host = "api"
path = "/sleep"
app = "echo"
timeouts = { request_deadline = "20s" }

[[route]]
host = "other"
app = "echo"
"""
    )
    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr
    daemon.wait_for_routing("echo")

    # The host's body limit holds for its routes, the route's own replaces
    # it, and a host that sets none has the node's.

    environ = json.loads(daemon.request("/info", host="api.test").body)["environ"]
    assert environ["muru.max_body_size"] == 1024
    environ = json.loads(daemon.request("/info", host="other.test").body)["environ"]
    assert environ["muru.max_body_size"] == 10 * 1024 * 1024
    assert post(daemon, "/", "api.test", 4096) == 413
    assert post(daemon, "/echo", "api.test", 4096) == 200
    assert post(daemon, "/echo", "other.test", 4096) == 200

    # So does its deadline: a request that takes two seconds is cut off at
    # the host's one second, and served by the route with its own.

    cut = daemon.request("/busy?seconds=2", host="api.test")
    assert cut.status == 504, cut.body
    assert daemon.request("/sleep?seconds=2", host="api.test").status == 200
    assert daemon.request("/busy?seconds=2", host="other.test").status == 200

    # The listings and the effective configuration say where each limit
    # comes from.

    hosts = daemon.muructl("host", "list").stdout
    assert "[max body 1KB] [queue_timeout 2s, request_deadline 1s]" in hosts, hosts
    routes = daemon.muructl("route", "list").stdout
    assert "[max body 1MB]" in routes and "[request_deadline 20s]" in routes, routes
    effective = daemon.muructl("config", "show", "--effective").stdout

    for line in [
        "route api /: max_body_size = 1KB (host api)",
        "route api /: timeouts.request_deadline = 1s (host api)",
        "route api /echo: max_body_size = 1MB (the route)",
        "route api /echo: timeouts.queue_timeout = 2s (host api)",
        "route api /sleep: timeouts.request_deadline = 20s (the route)",
        "route other /: max_body_size = 10MB (default)",
        "route other /: timeouts.request_deadline = 300s (default)",
    ]:
        assert line in effective, f"{line!r} missing from:\n{effective}"

    # The keys are checked when applied.

    config.write_text(
        config.read_text()
        .replace('max_body_size = "1KB"', 'max_body_size = "0"')
        .replace('request_deadline = "20s"', 'request_deadline = "soon"')
    )
    refused = apply(daemon, config)
    assert refused.returncode != 0
    errors = refused.stdout + refused.stderr
    assert "host api: max_body_size must be at least 1 byte" in errors, errors
    assert "timeouts.request_deadline:" in errors, errors


def test_a_host_redirects_to_its_canonical_name_and_chooses_its_access_log(
    daemon: Daemon, tmp_path: Path
) -> None:
    config = tmp_path / "hosts.toml"
    config.write_text(
        ECHO
        + """[host.shop]
names = ["shop.test", "www.shop.test"]
canonical = "www.shop.test"
canonical_redirect = true

[host.quiet]
names = ["quiet.test"]
access_log = "errors"
access_log_exclude = ["/missing-and-left-out"]

[[route]]
host = "shop"
app = "echo"

[[route]]
host = "quiet"
app = "echo"
"""
    )
    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr
    daemon.wait_for_routing("echo")

    # A request for another of the host's names is redirected to the
    # canonical one, on the port and with the target it came with.

    response = daemon.request("/info?x=1", host=f"shop.test:{daemon.port}")
    assert response.status == 307
    headers = {name.lower(): value for name, value in response.headers}
    assert headers["location"] == f"http://www.shop.test:{daemon.port}/info?x=1"
    bare = daemon.request("/", host="shop.test")
    assert {name.lower(): value for name, value in bare.headers}["location"] == (
        "http://www.shop.test/"
    )
    assert daemon.request("/", host="www.shop.test").status == 200
    assert "[redirects to www.shop.test]" in daemon.muructl("host", "list").stdout

    # The quiet host's requests are written only when they fail, less the
    # paths it leaves out; the other host's are all written.

    assert daemon.request("/text?quiet-ok", host="quiet.test").status == 200
    assert daemon.request("/missing-and-left-out", host="quiet.test").status == 404
    assert daemon.request("/missing", host="quiet.test").status == 404
    assert daemon.request("/text?loud-ok", host="www.shop.test").status == 200
    access_log = tmp_path / "logs" / "access.log"
    wait_for(lambda: access_log.exists() and "loud-ok" in access_log.read_text())
    written = access_log.read_text()
    assert '"/missing"' in written, written
    assert "quiet-ok" not in written and "missing-and-left-out" not in written, written

    # A redirect needs a name to redirect to.

    config.write_text(config.read_text().replace('canonical = "www.shop.test"\n', ""))
    refused = apply(daemon, config)
    assert refused.returncode != 0
    assert "host shop: canonical_redirect needs a canonical name" in (
        refused.stdout + refused.stderr
    )


def test_a_host_and_a_route_set_compression_over_the_nodes(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "hosts.toml"
    config.write_text(
        ECHO
        + """[host.raw]
names = ["raw.test"]
compression = { enabled = false }

[host.other]
names = ["other.test"]

[[route]]
host = "raw"
app = "echo"

[[route]]
host = "raw"
path = "/info"
app = "echo"
compression = { enabled = true, encodings = ["gzip"], min_size = "100" }

[[route]]
host = "other"
app = "echo"
"""
    )
    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr
    daemon.wait_for_routing("echo")

    def encoding(path: str, host: str) -> tuple[str | None, bytes]:
        response = daemon.request(path, host=host, headers={"Accept-Encoding": "br, gzip"})
        assert response.status == 200, response.body
        headers = {name.lower(): value for name, value in response.headers}
        return headers.get("content-encoding"), response.body

    # A host with no settings of its own has the node's, the host that
    # turns compression off sends its responses as they are, and its
    # route that turns it on again offers the encodings it names, for
    # bodies of the size it names.

    assert encoding("/text", "other.test")[0] == "br"
    coded, body = encoding("/text", "raw.test")
    assert coded is None and body.startswith(b"The quick brown fox")
    assert encoding("/info", "other.test")[0] is None, "under the node's 1KB"
    coded, body = encoding("/info", "raw.test")
    assert coded == "gzip"
    assert "environ" in json.loads(gzip.decompress(body))

    assert "[compression off]" in daemon.muructl("host", "list").stdout
    assert "[compression set]" in daemon.muructl("route", "list").stdout

    # The table is checked as the node's is.

    config.write_text(
        config.read_text().replace(
            "compression = { enabled = false }", 'compression = { level = "max" }'
        )
    )
    refused = apply(daemon, config)
    assert refused.returncode != 0
    assert 'host raw: compression: "max" is not a level' in refused.stdout + refused.stderr


def test_a_host_and_a_route_change_request_and_response_headers(
    daemon: Daemon, tmp_path: Path
) -> None:
    config = tmp_path / "hosts.toml"
    config.write_text(
        ECHO
        + """[host.shop]
names = ["shop.test"]
request_headers = { remove = ["X-Debug"], set = { X-Site = "shop", X-Tier = "host" } }

[host.shop.response_headers]
remove = ["Server"]
set = { X-Frame-Options = "DENY" }
add = { Link = ["</a.css>; rel=preload", "</b.js>; rel=preload"] }

[host.bare]
names = ["bare.test"]
response_headers = { set = { X-Content-Type-Options = "nosniff" } }

[[route]]
host = "shop"
app = "echo"

[[route]]
name = "shop-info"
host = "shop"
path = "/info"
app = "echo"
request_headers = { set = { X-Tier = "route" } }
response_headers = { remove = ["Link"], set = { X-Frame-Options = "SAMEORIGIN" } }

[[route]]
host = "bare"
path = "/only"
app = "echo"

[overlay.locked]
routes.shop-info = { response_headers = { set = { X-Locked = "yes" } } }
"""
    )
    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr
    daemon.wait_for_routing("echo")

    def headers_of(response: support.Response) -> dict[str, list[str]]:
        found: dict[str, list[str]] = {}

        for name, value in response.headers:
            found.setdefault(name.lower(), []).append(value)

        return found

    sent = {"X-Debug": "1", "X-Tier": "client", "X-Kept": "yes"}

    # The host's rules reach every route's requests and responses: the
    # application's response loses Server and gains the headers set and
    # added.

    headers = headers_of(daemon.request("/text", host="shop.test", headers=sent))
    assert "server" not in headers
    assert headers["x-frame-options"] == ["DENY"]
    assert headers["link"] == ["</a.css>; rel=preload", "</b.js>; rel=preload"]

    # The route's rules are applied after its host's, to the request the
    # application sees and to the response.

    response = daemon.request("/info", host="shop.test", headers=sent)
    environ = json.loads(response.body)["environ"]
    assert "HTTP_X_DEBUG" not in environ
    assert environ["HTTP_X_SITE"] == "shop"
    assert environ["HTTP_X_TIER"] == "route"
    assert environ["HTTP_X_KEPT"] == "yes"
    headers = headers_of(response)
    assert headers["x-frame-options"] == ["SAMEORIGIN"]
    assert "link" not in headers and "server" not in headers

    # A response the edge answers itself carries them too, a path no
    # route takes having its host's alone.

    missing = daemon.request("/missing", host="bare.test")
    assert missing.status == 404
    assert headers_of(missing)["x-content-type-options"] == ["nosniff"]

    listed = daemon.muructl("host", "list").stdout
    assert "[request headers: removes X-Debug; sets X-Site, X-Tier]" in listed, listed
    assert "[response headers: removes Server; sets X-Frame-Options; adds Link]" in listed

    # An overlay puts its table in place of the route's.

    daemon.muructl("overlay", "activate", "locked")
    wait_for(
        lambda: headers_of(daemon.request("/info", host="shop.test")).get("x-locked") == ["yes"]
    )
    headers = headers_of(daemon.request("/info", host="shop.test"))
    assert headers["x-frame-options"] == ["DENY"], "the host's, the route's table being replaced"
    assert len(headers["link"]) == 2

    # Headers the edge handles cannot be named, and a value is checked.

    for table, expected in [
        ('request_headers = { set = { Host = "x" } }', "request_headers.set: Host cannot be set"),
        ('response_headers = { remove = ["Content-Length"] }', "Content-Length cannot be set"),
        ('request_headers = { remove = ["X-Muru-Mode"] }', "X-Muru-Mode cannot be set"),
        ('response_headers = { set = { "X A" = "1" } }', '"X A" is not a header name'),
        ("response_headers = { add = { Link = [] } }", "response_headers.add.Link gives no value"),
    ]:
        broken = tmp_path / "broken.toml"
        broken.write_text(f'[host.broken]\nnames = ["broken.test"]\n{table}\n')
        refused = apply(daemon, broken)
        assert refused.returncode != 0, table
        assert expected in refused.stdout + refused.stderr, refused.stdout + refused.stderr


def test_a_host_in_maintenance_answers_503_for_every_route(daemon: Daemon, tmp_path: Path) -> None:
    files = tmp_path / "files"
    files.mkdir()
    (files / "page.html").write_text("<p>a file</p>")
    (tmp_path / "down.html").write_text("<p>back soon</p>")
    config = tmp_path / "hosts.toml"
    config.write_text(
        ECHO
        + f"""[host.shop]
names = ["shop.test"]
maintenance = {{ exempt_paths = ["/info"], bypass_cookie = "bypass=yes", retry_after = "90s" }}
error_pages = {{ "503:maintenance" = {{ file = "{tmp_path / "down.html"}" }} }}

[host.other]
names = ["other.test"]

[[route]]
host = "shop"
app = "echo"

[[route]]
host = "shop"
path = "/files"
static = "{files}"

[[route]]
host = "other"
app = "echo"
"""
    )
    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr
    daemon.wait_for_routing("echo")
    assert daemon.request("/files/page.html", host="shop.test").status == 200

    switched = daemon.muructl("maintenance", "on", "--host", "shop").stdout
    assert "host shop is in maintenance" in switched, switched

    # Every route of the host answers 503 maintenance with the host's
    # page, the static route included, and a path with no route too.

    for path in ["/", "/files/page.html", "/text"]:
        response = daemon.request(path, host="shop.test")
        assert response.status == 503, path
        headers = {name.lower(): value for name, value in response.headers}
        assert headers["retry-after"] == "90"
        assert response.body == b"<p>back soon</p>", response.body

    # What the host's exemptions let through is served, and so is every
    # other host, the same application included.

    assert daemon.request("/info", host="shop.test").status == 200
    cookie = {"Cookie": "bypass=yes"}
    assert daemon.request("/files/page.html", host="shop.test", headers=cookie).status == 200
    assert daemon.request("/", host="other.test").status == 200

    assert "[maintenance]" in daemon.muructl("host", "list").stdout
    listed = json.loads(daemon.muructl("host", "list", "--json").stdout)["hosts"]
    assert {host["name"]: host.get("maintenance", False) for host in listed} == {
        "other": False,
        "shop": True,
    }
    assert any(
        "maintenance.changed" in line and "host shop" in line and "maintenance on" in line
        for line in daemon.muructl("events").stdout.splitlines()
    )

    # It is off again when told, and the names are checked.

    assert "host shop serves as usual again" in (
        daemon.muructl("maintenance", "off", "--host", "shop").stdout
    )
    assert daemon.request("/", host="shop.test").status == 200
    unknown = daemon.muructl("maintenance", "on", "--host", "nowhere", check=False)
    assert unknown.returncode != 0 and "no virtual host named nowhere" in unknown.stderr
    both = daemon.muructl("maintenance", "on", "echo", "--host", "shop", check=False)
    assert both.returncode != 0
