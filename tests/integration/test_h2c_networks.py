"""Cleartext HTTP/2 limited to some clients (design specification section
6.13): a listener's `h2c` names the clients that may open HTTP/2 without
TLS, another client opening with HTTP/2 is answered 505, and a listener
without the key takes h2c from anyone.
"""

from __future__ import annotations

import socket
import subprocess
from pathlib import Path

from support import server as support
from support.daemon import start_daemon

PREFACE = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" + b"\x00\x00\x00\x04\x00\x00\x00\x00\x00"


def first_bytes(port: int) -> bytes:
    """What the edge answers to the HTTP/2 connection preface."""

    with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
        connection.sendall(PREFACE)
        return connection.recv(64)


def test_a_listener_keeps_h2c_to_the_clients_it_names(tmp_path: Path) -> None:
    open_port = support.free_port()
    limited_port = support.free_port()
    daemon = start_daemon(
        tmp_path,
        listeners=[(open_port, False)],
        config_extra=f"[[listener]]\naddress = '127.0.0.1:{limited_port}'\nh2c = ['10.0.0.0/8']\n",
    )

    try:
        # An HTTP/2 server answers the preface with its SETTINGS frame,
        # type 4 after the three bytes of length.

        answered = first_bytes(open_port)
        assert answered[3:4] == b"\x04", answered

        refused = first_bytes(limited_port)
        assert refused.startswith(b"HTTP/1.1 505 "), refused
    finally:
        daemon.stop()


def test_h2c_on_a_tls_listener_is_refused(tmp_path: Path) -> None:
    config = tmp_path / "muru.toml"
    config.write_text("[[listener]]\naddress = '127.0.0.1:0'\ntls = true\nh2c = ['loopback']\n")
    muructl = support.muructl_path()

    if muructl is None:
        support.unavailable("muructl is not built (cargo build -p muructl)")

    assert muructl is not None
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
            str(tmp_path / "run"),
            "--log-dir",
            str(tmp_path / "logs"),
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert completed.returncode != 0
    assert "h2c is for a listener without TLS" in completed.stderr, completed.stderr
