"""Many application threads in one process (design specification section
6.11): the stress test the free-threaded build is held to, which the regular
build runs as well. One worker with 32 threads serves 64 clients at once;
every response must be right, thread-locals must stay per thread, context
variables must not leak between requests, and the threads must really run
in parallel.
"""

from __future__ import annotations

import json
import time
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor

import pytest
from support import server as support

THREADS = 32
CLIENTS = 64
ROUNDS = 25


@pytest.fixture(scope="module")
def server() -> Iterator[support.Server]:
    instance = support.start("plain_wsgi:application", workers=1, threads=THREADS)
    try:
        yield instance
    finally:
        instance.stop()


def test_mixed_load_is_served_correctly(server: support.Server) -> None:
    def client(index: int) -> list[str]:
        problems = []
        for round_number in range(ROUNDS):
            payload = f"client {index} round {round_number}".encode() * 50
            echoed = server.post("/echo", payload)
            if echoed.status != 200 or echoed.body != payload:
                problems.append(f"echo {index}/{round_number}: {echoed.status}")
            context = server.get("/context")
            if json.loads(context.body) != {"inherited": None}:
                problems.append(f"context leaked: {context.body!r}")
            local = json.loads(server.get("/threadlocal").body)
            if local["count"] < 1:
                problems.append(f"thread-local count {local}")
        return problems

    with ThreadPoolExecutor(max_workers=CLIENTS) as pool:
        problems = [problem for found in pool.map(client, range(CLIENTS)) for problem in found]
    assert problems == []


def test_thread_locals_belong_to_their_threads(server: support.Server) -> None:
    # Each application thread counts its own requests: the counts seen
    # across many requests must come from at most THREADS distinct threads
    # and climb per thread.
    with ThreadPoolExecutor(max_workers=CLIENTS) as pool:
        seen = list(pool.map(lambda _i: json.loads(server.get("/threadlocal").body), range(400)))
    threads = {entry["thread"] for entry in seen}
    assert 1 < len(threads) <= THREADS
    assert max(entry["count"] for entry in seen) >= 400 // THREADS


def test_threads_run_in_parallel(server: support.Server) -> None:
    # 64 requests of 0.1 s on 32 threads take about 0.2 s in parallel and
    # 6.4 s if the threads serialised; the bound leaves room for slow CI.
    started = time.monotonic()
    with ThreadPoolExecutor(max_workers=CLIENTS) as pool:
        statuses = list(
            pool.map(lambda _i: server.get("/sleep?seconds=0.1").status, range(CLIENTS))
        )
    elapsed = time.monotonic() - started
    assert statuses == [200] * CLIENTS
    assert elapsed < 2.0, f"{elapsed:.2f}s for {CLIENTS} parallel sleeps"
