"""Threads of a mount's own (design specification section 6.7).

A WSGI mount's `threads` is a pool only it uses, or a reserved minimum
that borrows the shared threads up to a maximum. The edge holds slots per
pool, so a mount with its own threads runs while the shared ones are busy,
and a borrowing mount runs no more than its maximum at once. The shared
threads keep their number beside an ASGI mount.
"""

from __future__ import annotations

import json
import time
from collections.abc import Iterator
from concurrent.futures import Future, ThreadPoolExecutor
from typing import Any

import pytest
from support import server as support

POOLED = support.ROOT / "tests" / "fixtures" / "projects" / "pooled"


@pytest.fixture(scope="module")
def development() -> Iterator[support.Server]:
    instance = support.start(str(POOLED), workers=1, threads=None, chdir=None)

    try:
        yield instance
    finally:
        instance.stop()


def served(instance: support.Server, paths: list[str]) -> tuple[list[dict[str, Any]], float]:
    """Request the paths at once: what each answered, and how long they
    took together."""

    started = time.monotonic()

    with ThreadPoolExecutor(len(paths)) as executor:
        responses = list(executor.map(instance.get, paths))

    elapsed = time.monotonic() - started

    for response in responses:
        assert response.status == 200, response.body

    return [json.loads(response.body) for response in responses], elapsed


def shared_threads(instance: support.Server) -> set[int]:
    """The shared pool's two threads, each held by one of two requests."""

    answers, _ = served(instance, ["/?ms=500", "/?ms=500"])
    threads = {answer["thread"] for answer in answers}
    assert len(threads) == 2, answers
    assert all(answer["multithread"] for answer in answers)
    return threads


def test_the_shared_threads_keep_their_number_beside_an_asgi_mount(
    development: support.Server,
) -> None:
    assert development.get("/events").body == b"events"

    answers, elapsed = served(development, ["/?ms=1000", "/?ms=1000"])
    assert elapsed < 1.8, elapsed
    assert len({answer["thread"] for answer in answers}) == 2, answers


def test_a_mount_with_a_pool_of_its_own_runs_while_the_shared_threads_are_busy(
    development: support.Server,
) -> None:
    shared = shared_threads(development)

    with ThreadPoolExecutor(2) as executor:
        busy: list[Future[support.Response]] = [
            executor.submit(development.get, "/?ms=2500") for _ in range(2)
        ]
        time.sleep(0.3)
        answers, elapsed = served(development, ["/fixed?ms=1000", "/fixed?ms=1000"])
        assert all(future.result().status == 200 for future in busy)

    assert elapsed < 1.8, elapsed
    threads = {answer["thread"] for answer in answers}
    assert len(threads) == 2 and not threads & shared, (answers, shared)
    assert all(answer["multithread"] for answer in answers)


def test_a_mount_borrows_shared_threads_up_to_its_maximum(
    development: support.Server,
) -> None:
    shared = shared_threads(development)

    # Three at once with the shared threads idle: two run, one on the
    # mount's reserved thread and one on a borrowed shared thread, and the
    # third waits for either.

    answers, elapsed = served(development, ["/borrowing?ms=1000"] * 3)
    assert elapsed >= 2.0, elapsed
    threads = {answer["thread"] for answer in answers}
    assert threads & shared and threads - shared, (answers, shared)
    assert all(answer["multithread"] for answer in answers)

    # With the shared threads busy, the mount has its reserved one alone.

    with ThreadPoolExecutor(2) as executor:
        busy = [executor.submit(development.get, "/?ms=3000") for _ in range(2)]
        time.sleep(0.3)
        answers, elapsed = served(development, ["/borrowing?ms=800", "/borrowing?ms=800"])
        assert all(future.result().status == 200 for future in busy)

    assert elapsed >= 1.6, elapsed
    threads = {answer["thread"] for answer in answers}
    assert len(threads) == 1 and not threads & shared, (answers, shared)
