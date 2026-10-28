"""Limits of an ASGI mount's own (design specification section 6.7).

An ASGI mount with `concurrency` has slots of its own at the edge, so its
requests neither take the WSGI threads nor wait for them, and one with
`max_connections` holds that many open WebSockets apart from the requests
it runs: an accepted socket gives its request slot back, and a handshake
beyond the bound waits at the edge until the queue timeout.
"""

from __future__ import annotations

import time
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor

import pytest
from support import server as support
from websockets.exceptions import InvalidStatus
from websockets.sync.client import connect

LIMITED = support.ROOT / "tests" / "fixtures" / "projects" / "limited"


@pytest.fixture(scope="module")
def development() -> Iterator[support.Server]:
    instance = support.start(
        str(LIMITED), workers=1, threads=None, chdir=None, args=["--timeout", "queue=1s"]
    )

    try:
        yield instance
    finally:
        instance.stop()


def served(instance: support.Server, paths: list[str]) -> float:
    """Request the paths at once, all answered 200, and say how long they
    took together."""

    started = time.monotonic()

    with ThreadPoolExecutor(len(paths)) as executor:
        responses = list(executor.map(instance.get, paths))

    for response in responses:
        assert response.status == 200, response.body

    return time.monotonic() - started


def test_a_mount_runs_no_more_than_its_concurrency(development: support.Server) -> None:
    assert development.get("/ws/ping").body == b"asgi"

    # Three at once on a mount of two slots: the third waits for one.

    assert served(development, ["/ws/sleep?ms=600"] * 3) >= 1.2

    # The mount's slots are its own: two of its requests and two WSGI
    # requests all run at once, on the two threads and the two slots.

    assert served(development, ["/ws/sleep?ms=600"] * 2 + ["/?ms=600"] * 2) < 1.1


def test_open_websockets_are_bounded_apart_from_requests(development: support.Server) -> None:
    url = f"ws://127.0.0.1:{development.port}/ws/chat"

    with connect(url, open_timeout=10) as first, connect(url, open_timeout=10) as second:
        first.send("a")
        assert first.recv() == "echo:a"
        second.send("b")
        assert second.recv() == "echo:b"

        # Both sockets are open, and the mount's two request slots are
        # free again, so its requests still run.

        assert served(development, ["/ws/sleep?ms=100"] * 2) < 1.0

        # A third socket waits for a place among the two and is answered
        # 503 once the queue timeout is up.

        started = time.monotonic()

        with pytest.raises(InvalidStatus) as refused:
            connect(url, open_timeout=10)

        assert refused.value.response.status_code == 503
        assert time.monotonic() - started >= 0.9

    # With both closed a socket connects again.

    with connect(url, open_timeout=10) as third:
        third.send("c")
        assert third.recv() == "echo:c"
