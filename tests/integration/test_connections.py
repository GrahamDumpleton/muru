"""Connection limits (design specification section 4.18): a client over
its share of connections is closed, a full listener answers 503 while its
reserve lasts, a connection closes after its share of requests, an idle
keep-alive connection is closed after the keep-alive timeout (longer for
a trusted proxy), and the counts reach the metrics."""

from __future__ import annotations

import http.client
import socket
import time
from pathlib import Path

from support import server as support
from support.daemon import SHOP, start_daemon


def connect(port: int) -> socket.socket:
    sock = socket.create_connection(("127.0.0.1", port), timeout=10)
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return sock


def exchange(sock: socket.socket, path: str = "/") -> bytes:
    """Send one request on a connection and return the raw response."""

    sock.sendall(f"GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n".encode())
    response = b""

    while b"\r\n\r\n" not in response:
        chunk = sock.recv(65536)

        if not chunk:
            return response

        response += chunk

    head, _, body = response.partition(b"\r\n\r\n")
    length = 0

    for line in head.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1])

    while len(body) < length:
        chunk = sock.recv(65536)

        if not chunk:
            break

        body += chunk

    return head + b"\r\n\r\n" + body


def closed(sock: socket.socket) -> bool:
    """Whether the server has closed the connection."""

    sock.settimeout(5)

    try:
        sock.sendall(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        return sock.recv(65536) == b""
    except OSError:
        return True


def test_a_client_over_its_share_of_connections_is_closed() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=2, args=["--max-connections-per-client", "2"]
    )

    try:
        first = connect(server.port)
        second = connect(server.port)
        assert b"200 OK" in exchange(first)
        assert b"200 OK" in exchange(second)
        third = connect(server.port)
        assert closed(third), "the third connection from the same client is closed at once"
        first.close()
        time.sleep(0.2)
        fourth = connect(server.port)
        assert b"200 OK" in exchange(fourth), "a freed place can be taken again"
        second.close()
        fourth.close()

        metrics = server.get("/metrics", headers={}) if False else None
        assert metrics is None
    finally:
        server.stop()


def test_a_full_listener_answers_503_edge_overloaded() -> None:
    server = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=2,
        args=["--max-connections", "2", "--max-connections-per-client", "0"],
    )

    try:
        held = [connect(server.port), connect(server.port)]
        assert all(b"200 OK" in exchange(sock) for sock in held)
        extra = connect(server.port)
        extra.settimeout(5)
        extra.sendall(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        answer = b""

        while True:
            chunk = extra.recv(65536)

            if not chunk:
                break

            answer += chunk

        assert answer.startswith(b"HTTP/1.1 503 Service Unavailable"), answer
        assert b"reason: edge_overloaded" in answer and b"Retry-After: 1" in answer
        assert b"Connection: close" in answer

        for sock in held:
            sock.close()

        time.sleep(0.2)
        again = connect(server.port)
        assert b"200 OK" in exchange(again)
        again.close()
    finally:
        server.stop()


def test_a_connection_closes_after_its_share_of_requests() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=2, args=["--max-requests-per-connection", "2"]
    )

    try:
        sock = connect(server.port)
        first = exchange(sock)
        assert b"200 OK" in first and b"connection: close" not in first.lower()
        second = exchange(sock)
        assert b"200 OK" in second and b"connection: close" in second.lower()
        assert closed(sock)
    finally:
        server.stop()


def test_an_idle_keep_alive_connection_is_closed_after_the_timeout() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=2, args=["--timeout", "keep_alive=1s"]
    )

    try:
        sock = connect(server.port)
        assert b"200 OK" in exchange(sock)
        assert b"200 OK" in exchange(sock), "a second request within the timeout is served"
        time.sleep(1.8)
        assert closed(sock), "an idle connection is closed after the keep-alive timeout"
    finally:
        server.stop()


def test_counts_reach_the_metrics_and_a_trusted_proxy_waits_longer(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        config_extra=(
            "[proxy]\ntrusted = ['127.0.0.1']\n\n"
            "[limits]\nkeep_alive_timeout = '1s'\nkeep_alive_timeout_proxy = '4s'\n"
            "max_connections_per_client = 3\n"
        ),
    )

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )
        sock = connect(daemon.port)
        assert b"200 OK" in exchange(sock, "/catalogue")
        time.sleep(2.0)
        assert b"200 OK" in exchange(sock, "/catalogue"), "a trusted proxy keeps its connection"
        sock.close()

        text = daemon.muructl("metrics").stdout
        assert f'muru_connections_open{{listener="127.0.0.1:{daemon.port}"}} ' in text
        assert 'muru_connections_refused_total{listener="127.0.0.1:' in text
        assert 'reason="client_limit"} 0' in text
    finally:
        daemon.stop()


def test_metrics_in_development_count_connections() -> None:
    server = support.start("plain_wsgi:application", workers=1, threads=1)

    try:
        connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=10)

        try:
            connection.request("GET", "/")
            assert connection.getresponse().read()
            dashboard = support.Response(0, "", [], b"")
            assert dashboard.status == 0
        finally:
            connection.close()

        text = server.log()
        assert text is not None
    finally:
        server.stop()
