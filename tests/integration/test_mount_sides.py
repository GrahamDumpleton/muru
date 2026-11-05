"""How a worker with both interfaces counts requests (design
specification sections 5.1 and 6.7, ADR 0048).

The side that runs a request counts it: a WSGI request, the default
mount's or a named mount's, is held against the worker's threads, and an
ASGI request or an open WebSocket against its event loop's concurrency,
whichever the release's own entrypoint is. Neither side needs a setting
to stay out of the other's way.
"""

from __future__ import annotations

import shutil
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest
from support import server as support
from websockets.sync.client import connect

LIMITED = support.ROOT / "tests" / "fixtures" / "projects" / "limited"


def project(directory: Path, table: str) -> Path:
    """A project of the sleeping WSGI and ASGI applications with `table`."""

    directory.mkdir()
    shutil.copy(LIMITED / "limited.py", directory / "limited.py")
    (directory / "muru.toml").write_text(table)
    return directory


def statuses(instance: support.Server, paths: list[str]) -> tuple[list[int], float]:
    """Request the paths at once: their statuses and how long they took together."""

    started = time.monotonic()

    with ThreadPoolExecutor(len(paths)) as executor:
        responses = list(executor.map(instance.get, paths))

    return [response.status for response in responses], time.monotonic() - started


def test_an_asgi_mount_beside_a_wsgi_application_takes_no_thread(tmp_path: Path) -> None:
    mixed = project(
        tmp_path / "mixed",
        'entrypoint = "limited:wsgi_app"\nthreads = 2\n\n'
        '[mounts.live]\npath = "/live"\nentrypoint = "limited:asgi_app"\n',
    )
    instance = support.start(str(mixed), workers=1, threads=None, chdir=None)

    try:
        assert "of 2 threads and an event loop of concurrency 1000" in instance.log()

        # Eight requests to the mount and two to the application, each
        # of half a second, all run at once: the mount's are on the
        # loop, far beyond the two threads, and leave both threads free.

        answered, took = statuses(instance, ["/live/sleep?ms=500"] * 8 + ["/?ms=500"] * 2)
        assert answered == [200] * 10
        assert took < 1.2, took

        # Open WebSockets on the mount, more than there are threads, do
        # not hold the application's threads either.

        url = f"ws://127.0.0.1:{instance.port}/live/chat"

        with connect(url) as first, connect(url) as second, connect(url) as third:
            for socket in (first, second, third):
                socket.send("a")
                assert socket.recv() == "echo:a"

            answered, took = statuses(instance, ["/?ms=100"] * 2)
            assert answered == [200, 200]
            assert took < 1.0, took
    finally:
        instance.stop()


def test_a_wsgi_mount_beside_an_asgi_application_queues_for_a_thread(tmp_path: Path) -> None:
    mixed = project(
        tmp_path / "mixed",
        'entrypoint = "limited:asgi_app"\nthreads = 2\n\n'
        '[mounts.legacy]\npath = "/legacy"\nentrypoint = "limited:wsgi_app"\n',
    )
    instance = support.start(
        str(mixed), workers=1, threads=None, chdir=None, args=["--timeout", "queue=1s"]
    )

    try:
        assert "of 2 threads and an event loop of concurrency 1000" in instance.log()

        # Two of the mount's requests hold its two threads for longer
        # than the queue timeout, so a third waits at the edge for a
        # thread and is answered 503 when the timeout is up, where the
        # loop's concurrency would have let it into the worker to wait
        # there. The ASGI application is not held up meanwhile.

        with ThreadPoolExecutor(2) as executor:
            held = [executor.submit(instance.get, "/legacy/?ms=2500") for _ in range(2)]
            time.sleep(0.3)
            started = time.monotonic()
            answered, took = statuses(instance, ["/sleep?ms=100"] * 6)
            assert answered == [200] * 6
            assert took < 1.0, took
            queued = instance.get("/legacy/")
            assert queued.status == 503, queued.body
            assert 0.5 <= time.monotonic() - started < 2.0
            assert [request.result().status for request in held] == [200, 200]
    finally:
        instance.stop()


def test_limits_on_a_mount_detected_as_wsgi_fail_the_start(tmp_path: Path) -> None:
    wrong = project(
        tmp_path / "wrong",
        'entrypoint = "limited:asgi_app"\n\n'
        '[mounts.legacy]\npath = "/legacy"\nentrypoint = "limited:wsgi_app"\nconcurrency = 5\n',
    )

    with pytest.raises(RuntimeError) as failed:
        support.start(str(wrong), workers=1, threads=None, chdir=None)

    assert (
        "mount legacy: concurrency and max_connections are an ASGI mount's, which runs on "
        "the event loop, and limited:wsgi_app is WSGI, which runs on threads"
    ) in str(failed.value)
