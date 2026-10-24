"""Virtual hosts and TLS in the daemon (design specification sections 4.3 to 4.5).

Hosts and routes are declared in files and applied; the edge selects the
host by name (exact, wildcard, the default host, else 421), the longest
route prefix selects the application, TLS listeners pick each host's
certificate by SNI, and a changed certificate file takes effect with the
next apply that touches the host. A restarted daemon takes every listener
back.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tomllib
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from support import server as support
from support.daemon import FACTORY, SHOP, Daemon, start_daemon

APPS = support.ROOT / "tests" / "fixtures" / "apps"
OPENSSL = shutil.which("openssl")


def certificate(directory: Path, name: str, *names: str) -> tuple[Path, Path]:
    """A self-signed certificate for `names`, written as PEM files."""

    if OPENSSL is None:
        support.unavailable("openssl is not installed")
    cert = directory / f"{name}.pem"
    key = directory / f"{name}-key.pem"
    alternatives = ",".join(f"DNS:{candidate}" for candidate in names)
    subprocess.run(
        [
            OPENSSL,
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-days",
            "2",
            "-subj",
            f"/CN={names[0]}",
            "-addext",
            f"subjectAltName={alternatives}",
            "-keyout",
            str(key),
            "-out",
            str(cert),
        ],
        check=True,
        capture_output=True,
    )

    return cert, key


def release(app: str, source: Path, **settings: object) -> str:
    lines = [f"[app.{app}.release.r1]", f"source = '{source}'", f"python = '{sys.executable}'"]
    for key, value in settings.items():
        lines.append(f"{key} = '{value}'" if isinstance(value, str) else f"{key} = {value}")

    return "\n".join(lines) + "\n\n"


APPLICATIONS = (
    release("shop", SHOP)
    + release("factory", FACTORY, interface="wsgi")
    + release("echo", APPS, entrypoint="plain_wsgi:application")
)


def hosts_file(directory: Path, cert: Path, key: Path) -> Path:
    """Three applications, three hosts (one with TLS) and their routes.

    The shop host routes by prefix, the echo host has the certificate and
    reports its environ, and the other host is the default.
    """

    path = directory / "hosts.toml"
    path.write_text(
        APPLICATIONS
        + f"""[host.shop]
names = ["shop.test", "*.shop.test"]

[host.echo]
names = ["echo.test", "*.echo.test"]
tls = {{ certificate = "{cert.name}", key = "{key.name}" }}

[host.other]
names = ["other.test"]
default = true

[[route]]
host = "shop"
app = "shop"

[[route]]
host = "shop"
path = "/factory"
app = "factory"

[[route]]
host = "echo"
app = "echo"

[[route]]
host = "other"
app = "factory"
"""
    )

    return path


@pytest.fixture
def daemon(tmp_path: Path) -> Iterator[Daemon]:
    """A daemon with a plain listener and a TLS listener."""

    plain, secure = support.free_port(), support.free_port()
    instance = start_daemon(tmp_path, listeners=[(plain, False), (secure, True)])
    try:
        yield instance
    finally:
        instance.stop()


def apply(daemon: Daemon, path: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    return daemon.muructl("apply", "-f", str(path), *arguments, check=False)


def environ_of(response: support.Response) -> dict[str, Any]:
    assert response.status == 200, response.body
    environ: dict[str, Any] = json.loads(response.body)["environ"]
    return environ


def test_hosts_select_the_application_and_the_default_host_takes_the_rest(
    daemon: Daemon, tmp_path: Path
) -> None:
    cert, key = certificate(tmp_path, "echo", "echo.test", "*.echo.test")
    applied = apply(daemon, hosts_file(tmp_path, cert, key))
    assert applied.returncode == 0, applied.stdout + applied.stderr
    actions = [line.split()[0] for line in applied.stdout.splitlines()[1:]]
    assert actions.count("create-host") == 3 and actions.count("create-route") == 4

    # The shop host takes shop.test and any single label under it, its
    # /factory prefix goes to the factory application (on a segment
    # boundary), and the default host answers names no host claims.

    shop = daemon.request("/", host="shop.test")
    assert shop.status == 200 and json.loads(shop.body)["app"] == "shop"
    assert json.loads(daemon.request("/", host="EU.shop.test:1234").body)["app"] == "shop"
    assert "created_in" in json.loads(daemon.request("/factory/x", host="shop.test").body)
    assert json.loads(daemon.request("/factoryish", host="shop.test").body)["app"] == "shop"
    assert "created_in" in json.loads(daemon.request("/", host="other.test").body)
    assert "created_in" in json.loads(daemon.request("/", host="nobody.test").body)

    # SERVER_NAME is the name the request matched with.

    assert environ_of(daemon.request("/info", host="echo.test"))["SERVER_NAME"] == "echo.test"
    assert environ_of(daemon.request("/info", host="X.echo.test:9"))["SERVER_NAME"] == "x.echo.test"
    assert environ_of(daemon.request("/info", host="echo.test"))["HTTP_HOST"] == "echo.test"

    listing = daemon.muructl("host", "list").stdout
    assert "echo.test, *.echo.test" in listing and "(default)" in listing
    assert "/factory" in daemon.muructl("route", "list").stdout


def test_without_a_default_host_unknown_names_get_421_and_no_route_404(
    daemon: Daemon, tmp_path: Path
) -> None:
    config = tmp_path / "hosts.toml"
    config.write_text(
        release("shop", SHOP)
        + """[host.shop]
names = ["shop.test"]

[[route]]
host = "shop"
app = "shop"
"""
    )
    assert apply(daemon, config).returncode == 0
    assert daemon.request("/", host="shop.test").status == 200

    refused = daemon.request("/", host="nobody.test")
    assert refused.status == 421 and b"misdirected" in refused.body
    assert daemon.request("/").status == 421, "no Host header matches no host either"

    config.write_text(
        release("shop", SHOP)
        + """[host.shop]
names = ["shop.test"]

[[route]]
host = "shop"
path = "/app"
app = "shop"
"""
    )
    changed = apply(daemon, config, "--prune")
    assert changed.returncode == 0, changed.stdout + changed.stderr
    assert "remove-route" in changed.stdout, "the root route is pruned"
    assert daemon.request("/app/x", host="shop.test").status == 200
    missing = daemon.request("/elsewhere", host="shop.test")
    assert missing.status == 404 and b"no_route" in missing.body


def test_tls_listener_picks_the_certificate_by_name_and_redirects_when_asked(
    daemon: Daemon, tmp_path: Path
) -> None:
    cert, key = certificate(tmp_path, "echo", "echo.test", "*.echo.test")
    config = hosts_file(tmp_path, cert, key)
    assert apply(daemon, config).returncode == 0
    secure = daemon.https_port
    assert secure is not None

    environ = environ_of(daemon.request("/info", host="echo.test", port=secure, cafile=cert))
    assert environ["wsgi.url_scheme"] == "https"
    assert environ["muru.tls"]["server_name"] == "echo.test"
    assert environ["SERVER_PORT"] == str(secure)
    wildcard = daemon.request("/info", host="eu.echo.test", port=secure, cafile=cert)
    assert wildcard.status == 200

    # The shop host has no certificate; the only one loaded is offered,
    # and the client refuses it for the wrong name.

    with pytest.raises(OSError):
        daemon.request("/", host="shop.test", port=secure, cafile=cert)

    # A host that insists on HTTPS redirects plain requests to the TLS
    # listener, keeping the host and the target.

    config.write_text(
        config.read_text().replace(
            "[host.echo]\n", "[host.echo]\nhttps_redirect = true\ncanonical = 'echo.test'\n"
        )
    )
    redirected = apply(daemon, config)
    assert redirected.returncode == 0, redirected.stdout + redirected.stderr
    assert "update-host" in redirected.stdout
    response = daemon.request("/info?x=1", host="echo.test")
    assert response.status == 307
    headers = {name.lower(): value for name, value in response.headers}
    assert headers["location"] == f"https://echo.test:{secure}/info?x=1"
    assert daemon.request("/", host="shop.test").status == 200, "other hosts are not redirected"


def test_a_replaced_certificate_is_served_once_the_host_is_applied_again(
    daemon: Daemon, tmp_path: Path
) -> None:
    cert, key = certificate(tmp_path, "echo", "echo.test")
    config = hosts_file(tmp_path, cert, key)
    assert apply(daemon, config).returncode == 0
    secure = daemon.https_port
    assert secure is not None
    assert daemon.request("/", host="echo.test", port=secure, cafile=cert).status == 200

    stale = tmp_path / "stale.pem"
    stale.write_bytes(cert.read_bytes())
    certificate(tmp_path, "echo", "echo.test")
    assert cert.read_bytes() != stale.read_bytes()
    assert daemon.request("/", host="echo.test", port=secure, cafile=stale).status == 200

    # An apply that changes the host pushes a snapshot with the new file.

    config.write_text(
        config.read_text().replace(
            "[host.echo]\n", "[host.echo]\nuse_canonical_name = true\ncanonical = 'echo.test'\n"
        )
    )
    reapplied = apply(daemon, config)
    assert reapplied.returncode == 0, reapplied.stdout + reapplied.stderr
    assert daemon.request("/", host="echo.test", port=secure, cafile=cert).status == 200
    with pytest.raises(OSError):
        daemon.request("/", host="echo.test", port=secure, cafile=stale)


def test_export_round_trips_hosts_and_routes_and_prune_removes_them(
    daemon: Daemon, tmp_path: Path
) -> None:
    cert, key = certificate(tmp_path, "echo", "echo.test", "*.echo.test")
    config = hosts_file(tmp_path, cert, key)
    assert apply(daemon, config).returncode == 0

    exported = daemon.muructl("config", "export").stdout
    document = tomllib.loads(exported)
    assert sorted(document["host"]) == ["echo", "other", "shop"]
    assert document["host"]["echo"]["tls"]["certificate"] == str(cert)
    assert len(document["route"]) == 4
    (tmp_path / "export.toml").write_text(exported)
    again = apply(daemon, tmp_path / "export.toml")
    assert again.stdout.strip() == "muru: no changes (10 objects as declared)", again.stdout

    # One application's export carries only its routes and their hosts.

    only_shop = tomllib.loads(daemon.muructl("config", "export", "--app", "shop").stdout)
    assert list(only_shop["host"]) == ["shop"]
    assert [route["app"] for route in only_shop["route"]] == ["shop"]

    # Pruning with the hosts gone removes routes, then hosts, and the
    # edge goes back to serving everything.

    config.write_text(APPLICATIONS)
    pruned = apply(daemon, config, "--prune")
    assert pruned.returncode == 0, pruned.stdout + pruned.stderr
    actions = [line.split()[0] for line in pruned.stdout.splitlines() if "remove-" in line]
    assert actions == ["remove-route"] * 4 + ["remove-host"] * 3
    assert daemon.muructl("host", "list").stdout.strip() == "no virtual hosts"
    assert daemon.request("/", host="anything.test").status == 200


def test_invalid_hosts_and_routes_are_refused_together(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "hosts.toml"
    config.write_text(
        release("shop", SHOP)
        + """[host.shop]
names = ["shop.test", "bad/name"]
tls = { certificate = "missing.pem", key = "missing-key.pem" }
use_canonical_name = true

[host.twin]
names = ["Shop.Test"]
default = true

[host.empty]

[host.another]
default = true

[[route]]
host = "nowhere"
path = "relative"
app = "ghost"

[[route]]
host = "shop"
app = "shop"

[[route]]
host = "shop"
app = "shop"
"""
    )
    refused = apply(daemon, config)
    assert refused.returncode == 1
    errors = refused.stdout
    for expected in [
        'host shop: "bad/name" is not a host name',
        "host shop: ",
        "missing.pem",
        "host shop: use_canonical_name needs a canonical name",
        "host twin: shop.test is also a name of host shop",
        "host empty has no names and is not the default",
        "more than one host is the default",
        "route nowhere relative: the path must start with /",
        "route nowhere relative: no host named nowhere",
        "route nowhere relative: no application named ghost",
        "route shop / is declared twice",
    ]:
        assert expected in errors, f"{expected!r} missing from:\n{errors}"
    assert daemon.muructl("host", "list").stdout.strip() == "no virtual hosts"
    assert daemon.releases() == []


def test_https_redirect_without_a_tls_listener_serves_the_request(tmp_path: Path) -> None:
    plain = start_daemon(tmp_path)
    try:
        config = tmp_path / "hosts.toml"
        config.write_text(
            release("shop", SHOP)
            + """[host.shop]
names = ["shop.test"]
https_redirect = true

[[route]]
host = "shop"
app = "shop"
"""
        )
        applied = apply(plain, config)
        assert applied.returncode == 0, applied.stdout + applied.stderr
        assert plain.request("/", host="shop.test").status == 200
    finally:
        plain.stop()


def test_a_restarted_daemon_takes_both_listeners_back(tmp_path: Path) -> None:
    plain, secure = support.free_port(), support.free_port()
    first = start_daemon(tmp_path, listeners=[(plain, False), (secure, True)])
    try:
        cert, key = certificate(tmp_path, "echo", "echo.test")
        assert apply(first, hosts_file(tmp_path, cert, key)).returncode == 0
        assert first.request("/", host="echo.test", port=secure, cafile=cert).status == 200
        first.stop(shutdown=False)

        second = start_daemon(
            tmp_path, run_dir=first.run_dir, listeners=[(plain, False), (secure, True)]
        )
        try:
            assert "re-adopted the edge" in second.log()
            status = second.muructl("status").stdout
            assert f"https://127.0.0.1:{secure}" in status
            assert f"http://127.0.0.1:{plain}" in status
            assert second.request("/", host="echo.test").status == 200
            assert second.request("/", host="echo.test", port=secure, cafile=cert).status == 200
        finally:
            second.stop()
    finally:
        if first.process.poll() is None:
            first.stop()
