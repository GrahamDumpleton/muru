"""Metrics (design specification section 11.1).

The edge counts the requests it completes and reports its samples to the
daemon, which adds what it knows about the workers and serves the
Prometheus text exposition on the metrics listener and through muructl
metrics. A development run serves the same text from its dashboard.
"""

from __future__ import annotations

import http.client
import json
import os
import re
import signal
import sys
import time
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, daemon, start_daemon
from test_hosts import certificate

__all__ = ["daemon"]

TEXT_FORMAT = "text/plain; version=0.0.4; charset=utf-8"


def fetch(port: int, path: str) -> tuple[int, dict[str, str], str]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)

    try:
        connection.request("GET", path)
        response = connection.getresponse()
        headers = {name.lower(): value for name, value in response.getheaders()}
        return response.status, headers, response.read().decode()
    finally:
        connection.close()


@pytest.fixture
def scraped(tmp_path: Path) -> Iterator[tuple[Daemon, int]]:
    """A daemon with a metrics listener on a free port."""

    port = support.free_port()
    instance = start_daemon(tmp_path, arguments=["--metrics", f"127.0.0.1:{port}"])

    try:
        yield instance, port
    finally:
        instance.stop()


def test_requests_are_counted_and_served_to_a_scraper(scraped: tuple[Daemon, int]) -> None:
    daemon, port = scraped

    # A request before any release is answered by the edge itself, so it
    # is counted with the reason and without an application.

    assert daemon.request("/early").status == 503
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    assert daemon.request("/catalogue").status == 200
    assert daemon.request("/catalogue").status == 200

    text = daemon.muructl("metrics").stdout
    assert text.startswith("# HELP muru_requests_total "), text
    assert "# TYPE muru_requests_total counter\n" in text
    assert (
        'muru_requests_total{app="shop",release="r1",group="default",mount="default",'
        'status="2xx",client_class="default"} 2\n' in text
    ), text
    assert re.search(
        r'muru_requests_total\{group="default",mount="default",status="5xx",reason="[a-z_]+",'
        r'client_class="default"\} 1\n',
        text,
    ), text
    assert "# TYPE muru_request_duration_seconds histogram\n" in text
    labels = 'app="shop",release="r1",group="default"'
    assert (
        f'muru_request_duration_seconds_bucket{{{labels},phase="total",le="+Inf"}} 2\n' in text
    ), text
    assert f'muru_request_duration_seconds_count{{{labels},phase="queue"}} 2\n' in text
    assert 'muru_requests_in_flight{app="shop",release="r1"} 0\n' in text, text
    assert 'muru_queue_depth{app="shop",release="r1",group="default"} 0\n' in text, text
    assert 'muru_slots_busy{app="shop",release="r1",group="default"} 0\n' in text, text

    # The shop declares two processes of three threads.

    assert 'muru_slots_total{app="shop",release="r1",group="default"} 6\n' in text, text
    assert 'muru_workers_ready{app="shop",release="r1",group="default"} 2\n' in text, text
    assert re.search(r"muru_snapshot_version [1-9]\d*\n", text), text

    # The listener serves the same text at /metrics and nothing else.

    status, headers, body = fetch(port, "/metrics")
    assert status == 200
    assert headers["content-type"] == TEXT_FORMAT
    assert 'muru_requests_total{app="shop",release="r1"' in body
    assert fetch(port, "/")[0] == 404
    assert f"metrics  http://127.0.0.1:{port}/metrics" in daemon.muructl("status").stdout


def test_the_listener_comes_from_the_configuration_file(tmp_path: Path) -> None:
    port = support.free_port()
    daemon = start_daemon(
        tmp_path,
        listeners=[(support.free_port(), False)],
        config_extra=f"[metrics]\nlisten = '127.0.0.1:{port}'\n",
    )

    try:
        status, headers, body = fetch(port, "/metrics")
        assert status == 200
        assert headers["content-type"] == TEXT_FORMAT
        assert "muru_snapshot_version" in body
        assert "muru_requests_total" not in body
    finally:
        daemon.stop()


def test_nothing_is_served_unless_asked(daemon: Daemon) -> None:
    assert "metrics" not in daemon.muructl("status").stdout
    text = daemon.muructl("metrics").stdout
    assert "muru_snapshot_version" in text


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError("the condition was not met in time")

        time.sleep(0.2)


def test_worker_and_listener_families_are_exported(tmp_path: Path) -> None:
    """The families fed by the hosts' reports and events, and by the TLS
    acceptor: each worker's GIL waits, the workers recycled and exited by
    reason, the requests on ASGI loops, and the handshakes by result."""

    cert, key = certificate(tmp_path, "shop", "shop.test")
    plain, secure = support.free_port(), support.free_port()
    daemon = start_daemon(tmp_path, listeners=[(plain, False), (secure, True)])

    try:
        config = tmp_path / "node.toml"
        config.write_text(
            f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1
max_requests = 5
memory = {{ check_interval = "1s" }}

[app.asgi.release.r1]
source = '{support.FIXTURES}'
python = '{sys.executable}'
entrypoint = 'plain_asgi:application'
processes = 1

[host.shop]
names = ["shop.test"]
tls = {{ certificate = "{cert.name}", key = "{key.name}" }}
default = true

[[route]]
host = "shop"
app = "shop"

[[route]]
host = "shop"
path = "/asgi"
app = "asgi"
"""
        )
        applied = daemon.muructl("apply", "-f", str(config), check=False)
        assert applied.returncode == 0, applied.stdout + applied.stderr
        daemon.wait_for_routing("shop")

        # Six requests to a worker allowed five: the host recycles it.

        pid = json.loads(daemon.request("/info", host="shop.test").body)["pid"]

        for _ in range(6):
            assert daemon.request("/info", host="shop.test").status == 200

        wait_for(lambda: "worker.recycled" in daemon.events())

        # A handshake the client completes, and one it refuses once it
        # sees a certificate for another name.

        assert daemon.request("/info", host="shop.test", port=secure, cafile=cert).status == 200

        with pytest.raises(OSError):
            daemon.request("/info", host="shop.test", port=secure, cafile=cert, server_hostname="x")

        # A worker killed from outside exits on its own and is restarted.

        daemon.wait_for_routing("shop")
        pid = json.loads(daemon.request("/info", host="shop.test").body)["pid"]
        os.kill(pid, signal.SIGKILL)
        wait_for(lambda: "worker.exited" in daemon.events())

        def restarted() -> bool:
            response = daemon.request("/info", host="shop.test")
            return response.status == 200 and json.loads(response.body)["pid"] != pid

        wait_for(restarted)

        def exported() -> str:
            return daemon.muructl("metrics").stdout

        labels = 'app="shop",release="r1",group="default"'
        wait_for(lambda: f'muru_worker_exits_total{{{labels},reason="crashed"}} 1\n' in exported())
        text = exported()
        assert re.search(
            rf'muru_worker_recycles_total\{{{re.escape(labels)},reason="max_requests"\}} [1-9]\n',
            text,
        ), text
        assert "# TYPE muru_worker_recycles_total counter\n" in text
        assert "# TYPE muru_worker_exits_total counter\n" in text
        assert 'muru_async_slots_busy{app="asgi",release="r1",group="default"} 0\n' in text, text
        assert f"muru_async_slots_busy{{{labels}}} 0\n" in text, "a WSGI group holds none"
        listener = f'listener="127.0.0.1:{secure}"'
        assert f'muru_tls_handshakes_total{{{listener},result="completed"}} 1\n' in text, text
        assert f'muru_tls_handshakes_total{{{listener},result="failed"}} 1\n' in text, text

        # The worker's own histograms arrive with its memory report, once
        # the restarted worker has served something; a free-threaded
        # worker never waits for a GIL.

        if sys._is_gil_enabled():  # type: ignore[attr-defined]
            wait_for(lambda: f"muru_gil_wait_seconds_count{{{labels}," in exported())
            text = exported()
            assert "# TYPE muru_gil_wait_seconds histogram\n" in text
            bucket = (
                rf'muru_gil_wait_seconds_bucket\{{{re.escape(labels)},worker="0",le="0.0001"\}}'
            )
            assert re.search(bucket + r" \d+\n", text), text
            assert re.search(
                rf'muru_gil_wait_seconds_sum\{{{re.escape(labels)},worker="0"\}} [0-9.e-]+\n', text
            ), text

        wait_for(
            lambda: (
                'muru_loop_lag_seconds_count{app="asgi",release="r1",group="default",worker="0"}'
                in exported()
            )
        )
    finally:
        daemon.stop()
