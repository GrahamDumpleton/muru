"""HTTP upstreams (design specification section 4.13).

An `[upstream.NAME]` table names an unmanaged HTTP server that routes,
static fallbacks, traffic shares and authentication sub-requests can
target, over HTTP/1.1, h2c or h2 with TLS, by TCP or Unix socket. The
edge re-serialises the request after strict normalisation, spreads
requests over the addresses, ejects one whose requests fail and probes a
configured path; X-Muru headers from the upstream mean nothing unless it
is trusted.
"""

from __future__ import annotations

import http.client
import json
import shutil
import socket
import subprocess
import sys
import threading
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, start_daemon
from support.upstream import Upstream
from websockets.sync.server import Server, ServerConnection, serve

STATIC = support.ROOT / "tests" / "fixtures" / "static"
OPENSSL = shutil.which("openssl")


def call(
    port: int,
    path: str,
    headers: dict[str, str] | None = None,
    method: str = "GET",
    body: bytes | None = None,
    host: str = "site.test",
) -> support.Response:
    """One request with a Host header."""

    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)

    try:
        connection.request(method, path, body=body, headers={"Host": host, **(headers or {})})
        raw = connection.getresponse()
        return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
    finally:
        connection.close()


def report(response: support.Response) -> dict[str, object]:
    assert response.status == 200, response.body
    loaded: dict[str, object] = json.loads(response.body)
    return loaded


def certificate(directory: Path, name: str) -> tuple[Path, Path, Path]:
    """A certificate for `name` issued by a throwaway authority.

    Returns the authority's certificate, the server certificate and its
    key, as PEM files.
    """

    if OPENSSL is None:
        support.unavailable("openssl is not installed")
    ca = directory / "ca.pem"
    ca_key = directory / "ca-key.pem"
    cert = directory / f"{name}.pem"
    key = directory / f"{name}-key.pem"
    request = directory / f"{name}.csr"
    extensions = directory / f"{name}.ext"
    extensions.write_text(f"subjectAltName=DNS:{name}\nbasicConstraints=CA:FALSE\n")
    new_key = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes"]

    def openssl(*arguments: str) -> None:
        subprocess.run([OPENSSL, *arguments], check=True, capture_output=True)

    openssl(
        "req",
        "-x509",
        *new_key,
        "-days",
        "2",
        "-subj",
        "/CN=test authority",
        "-keyout",
        str(ca_key),
        "-out",
        str(ca),
    )
    openssl(
        "req", "-new", *new_key, "-subj", f"/CN={name}", "-keyout", str(key), "-out", str(request)
    )
    openssl(
        "x509",
        "-req",
        "-in",
        str(request),
        "-CA",
        str(ca),
        "-CAkey",
        str(ca_key),
        "-CAcreateserial",
        "-days",
        "2",
        "-extfile",
        str(extensions),
        "-out",
        str(cert),
    )
    return ca, cert, key


class Fixture:
    """A daemon, the upstream servers and a WebSocket echo server."""

    def __init__(self, base: Path) -> None:
        self.legacy = Upstream("legacy")
        self.other = Upstream("other")
        self.socket = Upstream("socket", unix=True)
        self.dead_port = support.free_port()
        self.ws_port = support.free_port()
        self.ws_server: Server = serve(echo, "127.0.0.1", self.ws_port)
        self.ws_thread = threading.Thread(target=self.ws_server.serve_forever, daemon=True)
        self.ws_thread.start()
        self.inner_port = support.free_port()
        self.tls_port = support.free_port()
        ca, cert, key = certificate(base, "inner.test")
        self.daemon = start_daemon(
            base,
            listeners=[
                (support.free_port(), False),
                (self.inner_port, False),
                (self.tls_port, True),
            ],
            config_extra="[limits]\nrequest_deadline = '3s'\n",
        )
        self.base = base
        self.ca = ca
        self.cert = cert
        self.key = key

    def config(self) -> str:
        plain = self.daemon.port
        return f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1

[upstream.legacy]
addresses = ['127.0.0.1:{self.legacy.port}']
health = {{ eject_after = 1, eject_for = '1s' }}

[upstream.trusted]
addresses = ['127.0.0.1:{self.legacy.port}']
trust_muru_headers = true

[upstream.socket]
addresses = ['{self.socket.address}']

[upstream.pair]
addresses = ['127.0.0.1:{self.dead_port}', '127.0.0.1:{self.other.port}']
health = {{ eject_after = 1, eject_for = '30s' }}

[upstream.probed]
addresses = ['127.0.0.1:{self.other.port}', '127.0.0.1:{self.legacy.port}']
health = {{ path = '/health', interval = '200ms', unhealthy_after = 1, healthy_after = 1 }}

[upstream.gone]
addresses = ['127.0.0.1:{self.dead_port}']

[upstream.ws]
addresses = ['127.0.0.1:{self.ws_port}']

[upstream.inner-h2c]
addresses = ['127.0.0.1:{self.inner_port}']
protocol = 'h2c'

[upstream.inner-h2]
addresses = ['127.0.0.1:{self.tls_port}']
protocol = 'h2'
tls = {{ ca = '{self.ca}', server_name = 'inner.test' }}

[host.site]
names = ['site.test']
listeners = ['127.0.0.1:{plain}']

[host.inner]
names = ['inner.test']
listeners = ['127.0.0.1:{self.inner_port}', '127.0.0.1:{self.tls_port}']
default = true
tls = {{ certificate = '{self.cert}', key = '{self.key}' }}

[[route]]
host = 'inner'
path = '/'
app = 'shop'

[[route]]
host = 'site'
path = '/'
app = 'shop'
accept_redirects_from = ['trusted']

[[route]]
host = 'site'
path = '/legacy/'
upstream = 'legacy'

[[route]]
host = 'site'
path = '/trusted/'
upstream = 'trusted'

[[route]]
host = 'site'
path = '/socket/'
upstream = 'socket'

[[route]]
host = 'site'
path = '/pair/'
upstream = 'pair'

[[route]]
host = 'site'
path = '/probed/'
upstream = 'probed'

[[route]]
host = 'site'
path = '/gone/'
upstream = 'gone'

[[route]]
host = 'site'
path = '/ws/'
upstream = 'ws'

[[route]]
host = 'site'
path = '/h2c/'
upstream = 'inner-h2c'

[[route]]
host = 'site'
path = '/h2/'
upstream = 'inner-h2'

[[route]]
host = 'site'
path = '/assets/'
static = '{STATIC}'
fallback = {{ upstream = 'legacy' }}

[[route]]
host = 'site'
path = '/split/'
app = 'shop'
traffic = {{ split = {{ 'upstream:legacy' = 100 }}, sticky = true }}

[[route]]
host = 'site'
path = '/half/'
app = 'shop'
traffic = {{ split = {{ 'upstream:legacy' = 50 }} }}

[[route]]
host = 'site'
path = '/guarded/'
app = 'shop'
auth = {{ upstream = 'legacy', path = '/auth', copy_headers = ['X-Auth-User'] }}

[[route]]
host = 'site'
path = '/legacy-guarded/'
upstream = 'legacy'
auth = {{ app = 'shop', path = '/auth-check', copy_headers = ['X-Auth-User'] }}
"""

    def stop(self) -> None:
        self.daemon.stop()
        self.ws_server.shutdown()

        for upstream in (self.legacy, self.other, self.socket):
            upstream.stop()


def echo(connection: ServerConnection) -> None:
    for message in connection:
        connection.send(f"echo:{message}")


@pytest.fixture(scope="module")
def site(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Fixture]:
    base = tmp_path_factory.mktemp("upstreams")
    fixture = Fixture(base)

    try:
        config = base / "site.toml"
        config.write_text(fixture.config())
        applied = fixture.daemon.muructl("apply", "-f", str(config), check=False)
        assert applied.returncode == 0, applied.stdout + applied.stderr
        yield fixture
    finally:
        fixture.stop()


def test_a_request_is_proxied_with_forwarding_headers(site: Fixture) -> None:
    response = call(
        site.daemon.port,
        "/legacy/items?x=1",
        {"X-Custom": "kept", "Bad_Name": "dropped", "X-Muru-Redirect": "/evil"},
        method="POST",
        body=b"payload",
    )
    seen = report(response)
    assert seen["upstream"] == "legacy"
    assert seen["method"] == "POST" and seen["path"] == "/legacy/items?x=1"
    assert seen["body"] == "payload"
    headers = seen["headers"]
    assert isinstance(headers, dict)
    assert headers["host"] == "site.test"
    assert headers["x-custom"] == "kept"
    assert headers["x-forwarded-for"] == "127.0.0.1"
    assert headers["x-forwarded-proto"] == "http"
    assert headers["x-forwarded-host"] == "site.test"
    assert headers["x-real-ip"] == "127.0.0.1"
    assert headers["x-request-id"] == response.header("X-Request-ID")
    assert headers["content-length"] == "7"
    assert "bad_name" not in headers and "x-muru-redirect" not in headers

    # The upstream's headers reach the client, less its X-Muru ones, and
    # the edge names itself.

    assert response.header("X-Served-By") == "legacy"
    assert response.header("X-Muru-Redirect") is None
    assert response.header("Server") == "muru"

    # The access log names the upstream as the application, the address
    # as the mount and upstream as the group.

    time.sleep(0.2)
    access = site.daemon.muructl("logs", "--access", "--json").stdout
    records = [json.loads(line) for line in access.splitlines() if line.startswith("{")]
    lines = [record for record in records if record["target"] == "/legacy/items?x=1"]
    assert len(lines) == 1, access
    assert lines[0]["app"] == "legacy"
    assert lines[0]["group"] == "upstream"
    assert lines[0]["mount"] == f"127.0.0.1:{site.legacy.port}"


def test_a_chunked_body_is_buffered_and_reframed(site: Fixture) -> None:
    connection = http.client.HTTPConnection("127.0.0.1", site.daemon.port, timeout=30)

    try:
        connection.putrequest("PUT", "/legacy/upload", skip_host=True)
        connection.putheader("Host", "site.test")
        connection.putheader("Transfer-Encoding", "chunked")
        connection.endheaders()
        connection.send(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n")
        raw = connection.getresponse()
        response = support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
    finally:
        connection.close()

    seen = report(response)
    assert seen["body"] == "hello world"
    headers = seen["headers"]
    assert isinstance(headers, dict)
    assert headers["content-length"] == "11"
    assert "transfer-encoding" not in headers


def test_ambiguous_framing_never_reaches_the_upstream(site: Fixture) -> None:
    # ADR 0027: a message with both Content-Length and Transfer-Encoding
    # is read by its Transfer-Encoding and re-framed, so the upstream sees
    # one length and no chunking; a Content-Length that is not a number
    # is refused outright.

    with socket.create_connection(("127.0.0.1", site.daemon.port), timeout=10) as raw:
        raw.sendall(
            b"POST /legacy/echo HTTP/1.1\r\nHost: site.test\r\nContent-Length: 3\r\n"
            b"Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
        )
        raw.settimeout(10)
        answer = b""

        while b"\r\n\r\n" not in answer or not answer.endswith(b"}"):
            chunk = raw.recv(65536)

            if not chunk:
                break

            answer += chunk

    assert answer.startswith(b"HTTP/1.1 200"), answer
    seen = json.loads(answer.split(b"\r\n\r\n", 1)[1])
    assert seen["body"] == "hello"
    assert seen["headers"]["content-length"] == "5"
    assert "transfer-encoding" not in seen["headers"]

    with socket.create_connection(("127.0.0.1", site.daemon.port), timeout=10) as raw:
        raw.sendall(b"POST /legacy/echo HTTP/1.1\r\nHost: site.test\r\nContent-Length: x\r\n\r\n")
        raw.settimeout(10)
        assert raw.recv(4096).startswith(b"HTTP/1.1 400")


def test_a_unix_socket_address_is_served(site: Fixture) -> None:
    seen = report(call(site.daemon.port, "/socket/path"))
    assert seen["upstream"] == "socket"
    assert seen["path"] == "/socket/path"


def test_a_dead_address_is_ejected_and_the_live_one_answers(site: Fixture) -> None:
    for _ in range(4):
        assert report(call(site.daemon.port, "/pair/x"))["upstream"] == "other"

    metrics = site.daemon.muructl("metrics").stdout
    dead = f'muru_upstream_health{{upstream="pair",address="127.0.0.1:{site.dead_port}"}} 0'
    live = f'muru_upstream_health{{upstream="pair",address="127.0.0.1:{site.other.port}"}} 1'
    assert dead in metrics and live in metrics, metrics

    response = call(site.daemon.port, "/gone/x", {"Accept": "application/json"})
    assert response.status == 502
    assert json.loads(response.body)["reason"] == "upstream_failed"


def test_probes_take_an_unhealthy_address_out_of_service(site: Fixture) -> None:
    def health(name: str, port: int) -> str:
        prefix = f'muru_upstream_health{{upstream="{name}",address="127.0.0.1:{port}"}} '

        for line in site.daemon.muructl("metrics").stdout.splitlines():
            if line.startswith(prefix):
                return line[len(prefix) :]

        return "missing"

    deadline = time.monotonic() + 10

    while site.other.probes == 0 and time.monotonic() < deadline:
        time.sleep(0.1)

    assert site.other.probes > 0, "the probe task runs"
    site.other.healthy = False
    deadline = time.monotonic() + 10

    while health("probed", site.other.port) != "0" and time.monotonic() < deadline:
        time.sleep(0.1)

    assert health("probed", site.other.port) == "0"
    assert health("probed", site.legacy.port) == "1"

    for _ in range(4):
        assert report(call(site.daemon.port, "/probed/x"))["upstream"] == "legacy"

    site.other.healthy = True
    deadline = time.monotonic() + 10

    while health("probed", site.other.port) != "1" and time.monotonic() < deadline:
        time.sleep(0.1)

    assert health("probed", site.other.port) == "1"


def test_a_slow_upstream_is_cut_off_at_the_deadline(site: Fixture) -> None:
    started = time.monotonic()
    response = call(site.daemon.port, "/legacy/slow", {"Accept": "application/json"})
    elapsed = time.monotonic() - started
    assert response.status == 504
    assert json.loads(response.body)["reason"] == "deadline"
    assert 2.5 < elapsed < 6, elapsed


def test_x_muru_headers_count_only_from_a_trusted_upstream(site: Fixture) -> None:
    untrusted = call(site.daemon.port, "/legacy/redirect")
    assert untrusted.status == 200 and untrusted.body == b"redirect", "served as it is"
    assert untrusted.header("X-Muru-Redirect") is None

    trusted = call(site.daemon.port, "/trusted/redirect")
    assert trusted.status == 200
    seen = json.loads(trusted.body)
    assert seen["internal"] is True
    assert seen["redirect_url"] == "/trusted/redirect"


def test_a_static_route_falls_back_to_an_upstream(site: Fixture) -> None:
    served = call(site.daemon.port, "/assets/fox.txt")
    assert served.status == 200
    assert served.body == (STATIC / "fox.txt").read_bytes()
    seen = report(call(site.daemon.port, "/assets/missing"))
    assert seen["upstream"] == "legacy" and seen["path"] == "/assets/missing"


def test_a_traffic_share_goes_to_an_upstream(site: Fixture) -> None:
    response = call(site.daemon.port, "/split/")
    seen = report(response)
    assert seen["upstream"] == "legacy"
    cookies = response.header_values("Set-Cookie")
    assert any(cookie.startswith("muru_release=upstream:legacy;") for cookie in cookies), cookies

    # The sticky cookie keeps the client there, and a preview pins it.

    seen = report(call(site.daemon.port, "/split/", {"Cookie": "muru_release=upstream:legacy"}))
    assert seen["upstream"] == "legacy"
    previewed = call(site.daemon.port, "/half/", {"Cookie": "muru_preview=upstream:legacy"})
    assert report(previewed)["upstream"] == "legacy"

    # Half the buckets go to the upstream, the rest to the release.

    targets = set()

    for _ in range(60):
        answer = json.loads(call(site.daemon.port, "/half/").body)
        targets.add("upstream" if "upstream" in answer else answer["release"])

    assert targets == {"upstream", "r1"}, targets


def test_an_upstream_decides_for_an_application_route(site: Fixture) -> None:
    refused = call(site.daemon.port, "/guarded/internal-info")
    assert refused.status == 401
    assert refused.header("WWW-Authenticate") == 'Bearer realm="up"'

    allowed = call(site.daemon.port, "/guarded/internal-info?q=1", {"Cookie": "session=ok"})
    seen = report(allowed)
    auth_headers = seen["auth_headers"]
    assert isinstance(auth_headers, dict)
    assert auth_headers["HTTP_X_AUTH_USER"] == "upstream:/guarded/internal-info?q=1"


def test_an_application_guards_an_upstream_route(site: Fixture) -> None:
    assert call(site.daemon.port, "/legacy-guarded/x").status == 401
    seen = report(call(site.daemon.port, "/legacy-guarded/x", {"Cookie": "session=ok"}))
    headers = seen["headers"]
    assert isinstance(headers, dict)
    assert headers["x-auth-user"] == "alice"


def test_a_websocket_is_relayed_to_an_upstream(site: Fixture) -> None:
    from websockets.sync.client import connect

    # The socket is opened by address and the URI names the virtual host,
    # which the Host header then carries.

    with connect(
        f"ws://site.test:{site.daemon.port}/ws/chat",
        sock=socket.create_connection(("127.0.0.1", site.daemon.port), timeout=10),
        open_timeout=10,
    ) as connection:
        connection.send("hello")
        assert connection.recv(timeout=10) == "echo:hello"


def test_h2c_and_h2_upstreams_reach_the_daemon_itself(site: Fixture) -> None:
    for prefix in ("/h2c/", "/h2/"):
        response = call(site.daemon.port, f"{prefix}internal-info?q=1", {"X-User": prefix})
        assert response.status == 200, (prefix, response.body)
        seen = json.loads(response.body)
        assert seen["internal"] is False
        assert seen["path"] == f"{prefix}internal-info"
        assert seen["query"] == "q=1"
        assert seen["x_user"] == prefix, seen


def test_listings_and_apply_checks(site: Fixture, tmp_path: Path) -> None:
    listed = site.daemon.muructl("upstream", "list").stdout
    assert "legacy" in listed and "h2c" in listed and "[probing /health]" in listed
    assert "[trusts X-Muru headers]" in listed
    routes = site.daemon.muructl("route", "list").stdout
    assert "/legacy/" in routes and "upstream legacy" in routes
    assert "else upstream legacy" in routes
    assert "[auth by upstream legacy /auth]" in routes

    def refused(text: str) -> str:
        config = tmp_path / "bad.toml"
        config.write_text(text)
        completed = site.daemon.muructl("apply", "-f", str(config), check=False)
        assert completed.returncode != 0, completed.stdout
        return completed.stdout + completed.stderr

    assert "no upstream named nowhere" in refused(
        "[[route]]\nhost = 'site'\npath = '/bad/'\nupstream = 'nowhere'\n"
    )
    assert "more than one of" in refused(
        "[[route]]\nhost = 'site'\npath = '/bad/'\nupstream = 'legacy'\napp = 'shop'\n"
    )
    assert "h2 with tls" in refused(
        "[upstream.bad]\naddresses = ['127.0.0.1:1']\nprotocol = 'h2c'\ntls = {}\n"
    )
    assert "reading /nonexistent.pem" in refused(
        "[upstream.bad]\naddresses = ['127.0.0.1:1']\ntls = { ca = '/nonexistent.pem' }\n"
    )
    assert "names upstream nowhere, which does not exist" in refused(
        "[[route]]\nhost = 'site'\npath = '/bad/'\napp = 'shop'\n"
        "traffic = { split = { 'upstream:nowhere' = 10 } }\n"
    )

    # Export writes the upstreams back.

    exported = site.daemon.muructl("config", "export").stdout
    assert "[upstream.legacy]" in exported and "[upstream.inner-h2]" in exported
