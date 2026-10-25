"""The daemon's dashboard and JSON gateway (design specification section
10.7): the node, its applications and releases, hosts and routes, recent
requests, events and log tails, as pages and as JSON on a loopback
address the daemon serves itself."""

from __future__ import annotations

import http.client
import json
import threading
import time
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon


def wait_for(predicate: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.05)


def fetch(port: int, path: str) -> tuple[int, dict[str, str], bytes]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)

    try:
        connection.request("GET", path)
        response = connection.getresponse()
        return response.status, {k.lower(): v for k, v in response.getheaders()}, response.read()
    finally:
        connection.close()


def fetch_json(port: int, path: str) -> Any:
    status, headers, body = fetch(port, path)
    assert status == 200, body
    assert headers["content-type"] == "application/json"
    return json.loads(body)


@pytest.fixture
def served(tmp_path: Path) -> Iterator[tuple[Daemon, int]]:
    """A daemon with its dashboard on a free loopback port."""

    port = support.free_port()
    instance = start_daemon(tmp_path, arguments=["--dashboard", f"127.0.0.1:{port}"])

    try:
        yield instance, port
    finally:
        instance.stop()


def test_pages_show_the_node_its_releases_requests_events_and_logs(
    served: tuple[Daemon, int],
) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    assert daemon.request("/catalogue").status == 200
    wait_for(lambda: "shop: served /catalogue" in daemon.muructl("logs", "shop").stdout)

    status, headers, body = fetch(port, "/")
    assert status == 200
    assert headers["content-type"].startswith("text/html")
    assert "default-src 'none'" in headers["content-security-policy"]
    page = body.decode()
    assert "<title>Muru: daemon</title>" in page
    assert f"http://127.0.0.1:{daemon.port}" in page
    assert "shop@r1" in page and "ready" in page
    assert "/catalogue" in page
    assert "release.ready" in page
    assert "shop: served /catalogue" in page

    for path in (
        "/fragment/node/overview",
        "/fragment/node/requests",
        "/fragment/node/events",
        "/fragment/node/logs",
    ):
        status, headers, body = fetch(port, path)
        assert status == 200, path
        assert headers["content-type"].startswith("text/html")
        assert body

    assert fetch(port, "/static/htmx.min.js")[0] == 200
    assert fetch(port, "/missing")[0] == 404
    assert f"dashboard http://127.0.0.1:{port}/" in daemon.muructl("status").stdout


def test_the_gateway_serves_the_same_figures_as_json(served: tuple[Daemon, int]) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    assert daemon.request("/one").status == 200

    status = fetch_json(port, "/v1/status")
    assert status["pid"] == daemon.process.pid
    assert status["listeners"] == [f"http://127.0.0.1:{daemon.port}"]
    assert status["snapshot_version"] >= 1
    assert status["summary"]["errors"] == 0

    apps = fetch_json(port, "/v1/apps")
    assert [app["name"] for app in apps] == ["shop"]
    assert apps[0]["current"] == "r1"
    assert apps[0]["releases"][0]["state"] == "ready"
    assert apps[0]["releases"][0]["serving"] is True
    assert apps[0]["releases"][0]["workers"] == 2
    assert apps[0]["releases"][0]["slots_total"] == 6

    releases = fetch_json(port, "/v1/apps/shop/releases")
    assert releases == apps[0]["releases"]
    assert fetch(port, "/v1/apps/none/releases")[0] == 404

    requests = fetch_json(port, "/v1/requests")
    assert requests[0]["target"] == "/one"
    assert requests[0]["release"] == "shop@r1"
    assert requests[0]["status"] == 200

    events = fetch_json(port, "/v1/events")
    assert events[0]["sequence"] > events[-1]["sequence"], "newest first"
    assert "release.ready" in [event["kind"] for event in events]
    assert fetch_json(port, "/v1/hosts") == []
    assert fetch_json(port, "/v1/routes") == []

    status, headers, body = fetch(port, "/metrics")
    assert status == 200
    assert headers["content-type"].startswith("text/plain; version=0.0.4")
    assert 'muru_requests_total{app="shop"' in body.decode()


def test_the_dashboard_is_loopback_only(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError, match="loopback"):
        start_daemon(tmp_path, arguments=["--dashboard", "0.0.0.0:0"])


def read_sse(port: int, path: str, wanted: str, timeout: float = 15.0) -> str:
    """Read a Server-Sent Events stream until a message of `wanted` type arrives."""

    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)

    try:
        connection.request("GET", path)
        response = connection.getresponse()
        assert response.status == 200
        assert response.getheader("Content-Type") == "text/event-stream"
        deadline = time.monotonic() + timeout
        collected = b""

        while time.monotonic() < deadline:
            chunk = response.read1(4096) if hasattr(response, "read1") else response.read(4096)

            if not chunk:
                break

            collected += chunk

            if (
                f"event: {wanted}\n".encode() in collected
                and b"\n\n" in collected.split(f"event: {wanted}\n".encode(), 1)[1]
            ):
                return collected.decode()

        raise AssertionError(f"no {wanted} event in {collected!r}")
    finally:
        connection.close()


def test_the_history_grows_and_streams_for_the_charts(served: tuple[Daemon, int]) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"

    # Points are differences between successive readings a second apart,
    # so requests keep flowing while the first ones are waited for.

    def recorded() -> bool:
        for _ in range(20):
            assert daemon.request("/catalogue").status == 200

        history = fetch_json(port, "/v1/history?range=1h")
        points = history["points"]
        return len(points) >= 2 and sum(point[1] for point in points) > 0

    wait_for(recorded)
    history = fetch_json(port, "/v1/history?range=1h")
    assert history["range"] == "1h" and history["step_s"] == 1
    points = history["points"]
    assert len(points) >= 2
    assert all(len(point) == 7 for point in points)
    assert points[-1][0] > points[0][0], "seconds in order"
    assert any(point[6] == 2 for point in points), "two workers listening"
    assert fetch_json(port, "/v1/history?range=24h")["step_s"] == 60

    stream = read_sse(port, "/v1/history/stream", "point")
    assert "event: point\ndata: [" in stream

    # The page carries the charts and their script, which the policy
    # allows alongside the stylesheet.

    status, headers, body = fetch(port, "/")
    assert status == 200
    page = body.decode()
    assert 'id="charts"' in page and "/static/charts.js" in page and "/static/uplot.min.js" in page
    assert "style-src 'self' 'unsafe-inline'" in headers["content-security-policy"]

    for asset, kind in [
        ("/static/uplot.min.js", "text/javascript"),
        ("/static/uplot.min.css", "text/css"),
        ("/static/sse.js", "text/javascript"),
        ("/static/charts.js", "text/javascript"),
    ]:
        status, headers, body = fetch(port, asset)
        assert status == 200 and headers["content-type"] == kind and body, asset


def test_events_stream_as_rows_and_json(served: tuple[Daemon, int]) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    outcome: list[str] = []

    def listen() -> None:
        outcome.append(read_sse(port, "/v1/events/stream", "json"))

    listener = threading.Thread(target=listen)
    listener.start()
    time.sleep(0.5)
    assert daemon.muructl("drain", "shop@r1").stdout.strip() == "muru: release shop@r1 is stopped"
    listener.join(timeout=20)
    assert outcome, "no event arrived on the stream"
    text = outcome[0]
    assert "event: event\ndata: <tr class=" in text
    assert '"kind":"release.' in text or '"kind":"traffic.' in text
    page = fetch(port, "/")[2].decode()
    assert 'sse-connect="/v1/events/stream"' in page and 'id="event-rows"' in page
