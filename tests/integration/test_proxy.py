"""Trusted proxies (design specification section 4.7).

Nothing is trusted unless the daemon's `[proxy]` table says so: forwarding
headers from any other peer are removed before the application sees them.
From a trusted peer the configured header types give the client address,
the scheme, the host, the port and the path prefix, which reach the
application as REMOTE_ADDR, wsgi.url_scheme, HTTP_HOST, SERVER_PORT and
SCRIPT_NAME (client, scheme, server and root_path for ASGI), with the
proxy itself in muru.proxy_addr.
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import SHOP, Daemon, start_daemon

APPS = support.ROOT / "tests" / "fixtures" / "apps"


def deploy_echo(daemon: Daemon, asgi: bool = False) -> None:
    """Deploy the plain WSGI or ASGI fixture, which reports what it sees."""

    entrypoint = "plain_asgi:application" if asgi else "plain_wsgi:application"
    config = daemon.base / "echo.toml"
    config.write_text(
        f"[app.echo.release.r1]\nsource = '{APPS}'\nentrypoint = '{entrypoint}'\n"
        f"python = '{sys.executable}'\n"
    )
    completed = daemon.muructl("apply", "-f", str(config))
    assert "ready and serving" in completed.stdout, completed.stdout


def environ_of(daemon: Daemon, headers: dict[str, str]) -> dict[str, Any]:
    response = daemon.request("/info", headers=headers)
    assert response.status == 200, response.body
    environ: dict[str, Any] = json.loads(response.body)["environ"]
    return environ


def with_proxy(tmp_path: Path, table: str) -> Daemon:
    return start_daemon(tmp_path, config_extra=table)


def test_nothing_is_trusted_by_default(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)

    try:
        deploy_echo(daemon)
        environ = environ_of(
            daemon,
            {
                "X-Forwarded-For": "203.0.113.5",
                "X-Forwarded-Proto": "https",
                "X-Real-IP": "203.0.113.6",
                "Forwarded": "for=203.0.113.7",
            },
        )

        # The headers are gone and the peer is the client.

        assert environ["REMOTE_ADDR"] == "127.0.0.1"
        assert environ["wsgi.url_scheme"] == "http"
        assert "muru.proxy_addr" not in environ
        assert not any(key.startswith("HTTP_X_FORWARDED") for key in environ)
        assert "HTTP_X_REAL_IP" not in environ and "HTTP_FORWARDED" not in environ
    finally:
        daemon.stop()


def test_trusted_loopback_resolves_client_scheme_host_port_and_prefix(tmp_path: Path) -> None:
    daemon = with_proxy(
        tmp_path,
        """
[proxy]
trusted = ["loopback", "10.0.0.0/8"]
headers = [
    "x-forwarded-for", "x-forwarded-proto", "x-forwarded-host",
    "x-forwarded-port", "x-forwarded-prefix",
]
""",
    )

    try:
        deploy_echo(daemon)
        environ = environ_of(
            daemon,
            {
                "X-Forwarded-For": "203.0.113.5, 10.1.1.1",
                "X-Forwarded-Proto": "https",
                "X-Forwarded-Host": "shop.example.com",
                "X-Forwarded-Port": "443",
                "X-Forwarded-Prefix": "/shop",
                "X-Real-IP": "198.51.100.9",
            },
        )

        # Recursive by default: the chain is walked past the trusted
        # 10.1.1.1 to the client. The proxy that connected is kept beside.

        assert environ["REMOTE_ADDR"] == "203.0.113.5"
        assert environ["muru.proxy_addr"] == "127.0.0.1"
        assert environ["wsgi.url_scheme"] == "https"
        assert environ["HTTPS"] == "on"
        assert environ["HTTP_HOST"] == "shop.example.com"
        assert environ["SERVER_PORT"] == "443"
        assert environ["SCRIPT_NAME"] == "/shop"
        assert environ["PATH_INFO"] == "/info"

        # Configured headers stay, other forwarding headers go.

        assert environ["HTTP_X_FORWARDED_FOR"] == "203.0.113.5, 10.1.1.1"
        assert "HTTP_X_REAL_IP" not in environ

        # Without forwarding headers nothing changes.

        plain = environ_of(daemon, {})
        assert plain["REMOTE_ADDR"] == "127.0.0.1"
        assert "muru.proxy_addr" not in plain
        assert plain["SCRIPT_NAME"] == ""
    finally:
        daemon.stop()


def test_rightmost_address_and_consumed_headers(tmp_path: Path) -> None:
    daemon = with_proxy(
        tmp_path,
        """
[proxy]
trusted = ["127.0.0.1"]
recursive = false
forwarded_headers = "consume"
""",
    )

    try:
        deploy_echo(daemon)
        environ = environ_of(
            daemon,
            {
                "X-Forwarded-For": "203.0.113.5, 10.1.1.1",
                "Forwarded": "for=198.51.100.7;proto=https",
            },
        )

        # The default header types include Forwarded, read after
        # X-Forwarded-For, so its nearest element decides; with consume
        # the application sees none of the headers.

        assert environ["REMOTE_ADDR"] == "198.51.100.7"
        assert environ["wsgi.url_scheme"] == "https"
        assert "HTTP_X_FORWARDED_FOR" not in environ
        assert "HTTP_FORWARDED" not in environ

        only_for = environ_of(daemon, {"X-Forwarded-For": "203.0.113.5, 10.1.1.1"})
        assert only_for["REMOTE_ADDR"] == "10.1.1.1", "not recursive: the rightmost"
    finally:
        daemon.stop()


def test_asgi_scope_sees_the_resolved_client(tmp_path: Path) -> None:
    daemon = with_proxy(tmp_path, '\n[proxy]\ntrusted = ["loopback"]\n')

    try:
        deploy_echo(daemon, asgi=True)
        response = daemon.request(
            "/info",
            headers={
                "X-Forwarded-For": "203.0.113.5",
                "X-Forwarded-Proto": "https",
                "X-Forwarded-Host": "shop.example.com",
            },
        )
        assert response.status == 200, response.body
        report = json.loads(response.body)
        scope = report["scope"]

        assert scope["client"] == ["203.0.113.5", 0]
        assert scope["scheme"] == "https"
        assert report["extensions"]["muru"]["proxy_addr"] == "127.0.0.1"
        assert ["host", "shop.example.com"] in [list(pair) for pair in scope["headers"]]
    finally:
        daemon.stop()


def test_forwarded_host_selects_the_virtual_host(tmp_path: Path) -> None:
    daemon = with_proxy(tmp_path, '\n[proxy]\ntrusted = ["loopback"]\n')

    try:
        config = daemon.base / "hosts.toml"
        config.write_text(
            f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'

[host.shop]
names = ["shop.example.com"]

[[route]]
host = "shop"
app = "shop"
"""
        )
        applied = daemon.muructl("apply", "-f", str(config))
        assert "ready and serving" in applied.stdout, applied.stdout

        assert daemon.request("/", host="proxy.internal").status == 421
        forwarded = daemon.request(
            "/", host="proxy.internal", headers={"X-Forwarded-Host": "shop.example.com"}
        )
        assert forwarded.status == 200, forwarded.body
    finally:
        daemon.stop()


def test_a_bad_proxy_table_refuses_to_start(tmp_path: Path) -> None:
    # The run directory is short, so the proxy table is what refuses.

    import subprocess

    muructl = support.muructl_path()
    assert muructl is not None
    config = tmp_path / "muru.toml"
    config.write_text('[proxy]\ntrusted = ["everywhere"]\n')
    completed = subprocess.run(
        [
            str(muructl),
            "daemon",
            "run",
            "--config",
            str(config),
            "--state-dir",
            str(tmp_path / "state"),
            "--run-dir",
            tempfile.mkdtemp(prefix="muru-run-", dir="/tmp"),
            "--log-dir",
            str(tmp_path / "logs"),
            "--listen",
            f"127.0.0.1:{support.free_port()}",
        ],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )

    assert completed.returncode != 0
    assert "everywhere" in completed.stderr
