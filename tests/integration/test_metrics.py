"""Metrics (design specification section 11.1).

The edge counts the requests it completes and reports its samples to the
daemon, which adds what it knows about the workers and serves the
Prometheus text exposition on the metrics listener and through muructl
metrics. A development run serves the same text from its dashboard.
"""

from __future__ import annotations

import http.client
import re
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, daemon, start_daemon

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
