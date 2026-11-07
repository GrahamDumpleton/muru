"""`muructl requests` (design specification section 11.7, issue 379): the
requests in flight, queued at the edge or running, counted by worker group
and listed, as `muructl websockets` does for open sockets.
"""

from __future__ import annotations

import json
import threading
import time
from collections.abc import Callable

from support import server as support
from test_run_admin import muructl


def wait_for(predicate: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def in_background(server: support.Server, path: str, count: int) -> list[threading.Thread]:
    threads = [threading.Thread(target=server.get, args=(path,)) for _ in range(count)]

    for thread in threads:
        thread.start()

    return threads


def test_requests_counts_queued_and_running_by_group_and_lists_them() -> None:
    runtime = support.short_dir("muru-rt-")
    server = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=1,
        env={"MURU_RUN_DIR": str(runtime)},
    )

    assert server.wait_for_log("admin socket"), server.log()

    def words(*command: str) -> str:
        return " ".join(muructl(runtime, *command, cwd=support.FIXTURES).stdout.split())

    try:
        # A release with nothing in flight still has a row per group,
        # with the thread columns of a WSGI release and no loop columns.

        idle = words("requests")
        assert idle.startswith("RELEASE GROUP QUEUED QUEUED-OLDEST THREADS THREAD-OLDEST ")
        assert "LOOP" not in idle
        assert "plain-wsgi@r1 default 0 - 0/1 -" in idle
        assert words("requests", "--list") == "no requests in flight"

        # One thread, three slow requests: one runs, two queue, and the
        # ages are as of now, from each request's arrival. The first goes
        # alone and holds the thread before the others are sent, since
        # requests that arrive together can reach the thread in another
        # order than the one they were stamped in (issue 423).

        waiting = in_background(server, "/sleep?seconds=4", 1)

        try:
            wait_for(lambda: " 1/1 " in words("requests"))
            waiting += in_background(server, "/sleep?seconds=4", 2)
            wait_for(lambda: " 2 " in words("requests") and " 1/1 " in words("requests"))
            time.sleep(1.1)
            answer = json.loads(muructl(runtime, "requests", "--json", cwd=support.FIXTURES).stdout)
            assert answer["in_flight"] == 3
            (group,) = answer["groups"]
            assert (group["queued"], group["threads_busy"], group["threads_of"]) == (2, 1, 1)
            assert group["queued_oldest_ms"] > 1000 and group["threads_oldest_ms"] > 1000
            assert group["threads"] and not group["event_loop"]

            listed = muructl(
                runtime, "requests", "--list", cwd=support.FIXTURES
            ).stdout.splitlines()
            assert listed[0].split() == [
                "AGE", "STATE", "KIND", "WORKER", "REQUEST", "CLIENT", "REQUEST-ID",
            ]  # fmt: skip
            rows = [line.split() for line in listed[1:]]
            assert [row[1] for row in rows] == ["running", "queued", "queued"], listed
            assert rows[0][2:4] == ["wsgi", "0"] and rows[1][2:4] == ["-", "-"]
            assert all(row[4:6] == ["GET", "/sleep?seconds=4"] for row in rows)

            # The listing is the oldest first, by each request's arrival.

            listing = muructl(runtime, "requests", "--list", "--json", cwd=support.FIXTURES)
            ages = [int(request["age_ms"]) for request in json.loads(listing.stdout)["requests"]]
            assert len(ages) == 3 and ages == sorted(ages, reverse=True), ages

            limited = muructl(
                runtime, "requests", "--list", "--limit", "1", cwd=support.FIXTURES
            ).stdout
            assert "(1 of 3 in flight shown; --limit shows more)" in limited
        finally:
            for thread in waiting:
                thread.join()

        wait_for(lambda: "plain-wsgi@r1 default 0 - 0/1 -" in words("requests"))
    finally:
        server.stop()


def test_requests_on_an_event_loop_count_against_the_loop() -> None:
    runtime = support.short_dir("muru-rt-")
    server = support.start(
        "plain_asgi:application",
        workers=1,
        threads=None,
        env={"MURU_RUN_DIR": str(runtime)},
    )

    assert server.wait_for_log("admin socket"), server.log()

    def words(*command: str) -> str:
        return " ".join(muructl(runtime, *command, cwd=support.FIXTURES).stdout.split())

    try:
        idle = words("requests", "plain-asgi")
        assert idle.startswith("RELEASE GROUP QUEUED QUEUED-OLDEST LOOP LOOP-OLDEST")
        assert "THREADS" not in idle

        # A streaming response is on the loop and says so, for as long as
        # it streams; an ordinary one is `asgi`.

        waiting = in_background(server, "/events?delay=2", 1) + in_background(
            server, "/sleep?seconds=3", 1
        )

        try:
            wait_for(lambda: "asgi streaming" in words("requests", "--list"))
            listed = words("requests", "--list")
            assert "running asgi streaming 0 GET /events?delay=2" in listed
            assert "running asgi 0 GET /sleep?seconds=3" in listed
            counted = words("requests")
            assert "plain-asgi@r1 default 0 - 2 " in counted, counted
        finally:
            for thread in waiting:
                thread.join()
    finally:
        server.stop()
