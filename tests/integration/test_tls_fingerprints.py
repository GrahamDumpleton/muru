"""TLS fingerprints (design specification section 4.14): the edge reads
the JA4 of each TLS connection's ClientHello, the application finds it in
`muru.tls`, and a client class rule's `ja4` or `ja4_list` matches it, a
list file being read again when it changes.
"""

from __future__ import annotations

import http.client
import json
import re
import shutil
import socket
import ssl
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import start_daemon

APPS = support.ROOT / "tests" / "fixtures" / "apps"
OPENSSL = shutil.which("openssl")
JA4 = re.compile(r"^t1[0-3][di]\d{4}[0-9a-z]{2}_[0-9a-f]{12}_[0-9a-f]{12}$")

CLASSES = """
[[client_class]]
name = 'fingerprinted'
ja4_list = 'lists/ja4.txt'
"""


def certificate(directory: Path) -> None:
    """A self-signed certificate for echo.test, as echo.pem and its key."""

    if OPENSSL is None:
        support.unavailable("openssl is not installed")

    assert OPENSSL is not None
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
            "/CN=echo.test",
            "-addext",
            "subjectAltName=DNS:echo.test",
            "-keyout",
            str(directory / "echo-key.pem"),
            "-out",
            str(directory / "echo.pem"),
        ],
        check=True,
        capture_output=True,
    )


def environ_over_tls(port: int) -> dict[str, Any]:
    """The environ of a request to echo.test over TLS, the name sent in
    the ClientHello."""

    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    raw = socket.create_connection(("127.0.0.1", port), timeout=10)
    connection = http.client.HTTPConnection("echo.test", port, timeout=10)
    connection.sock = context.wrap_socket(raw, server_hostname="echo.test")

    try:
        connection.request("GET", "/info")
        response = connection.getresponse()
        body = response.read()
        assert response.status == 200, body
    finally:
        connection.close()

    environ: dict[str, Any] = json.loads(body)["environ"]
    return environ


def test_a_class_matches_the_fingerprint_the_application_sees(tmp_path: Path) -> None:
    certificate(tmp_path)
    (tmp_path / "lists").mkdir()
    listed = tmp_path / "lists" / "ja4.txt"
    listed.write_text("# none yet\n")
    plain, secure = support.free_port(), support.free_port()
    daemon = start_daemon(
        tmp_path, listeners=[(plain, False), (secure, True)], config_extra=CLASSES
    )

    try:
        config = tmp_path / "echo.toml"
        config.write_text(
            f"""[app.echo.release.r1]
source = '{APPS}'
python = '{sys.executable}'
entrypoint = 'plain_wsgi:application'

[host.echo]
names = ['echo.test']
tls = {{ certificate = 'echo.pem', key = 'echo-key.pem' }}

[[route]]
host = 'echo'
app = 'echo'
"""
        )
        daemon.muructl("apply", "-f", str(config))
        daemon.muructl("release", "wait", "echo@r1", "--timeout", "60")

        environ = environ_over_tls(secure)
        fingerprint = environ["muru.tls"]["ja4"]
        assert JA4.match(fingerprint), fingerprint
        assert fingerprint[3] == "d", fingerprint
        assert environ["muru.client_class"] == "default"

        # Named in the list, the same client's next connection is of the
        # class; the list is looked at every second.

        listed.write_text(f"# this test's client\n{fingerprint}\n")
        deadline = time.monotonic() + 15

        while environ["muru.client_class"] != "fingerprinted" and time.monotonic() < deadline:
            time.sleep(0.5)
            environ = environ_over_tls(secure)

        assert environ["muru.client_class"] == "fingerprinted"
        assert "client_class.list_reloaded" in daemon.events()
    finally:
        daemon.stop()
