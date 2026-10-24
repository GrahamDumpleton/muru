"""Sustained load for the zero-failed-request scenarios (design specification
sections 14.5 and 15.2).

Client threads send requests on kept-alive connections, as real clients do,
for as long as a scenario deploys, rolls back, drains or restarts the daemon
underneath them, and count what came back: every status, every connection
error and the slowest request. A scenario then asserts that nothing failed.
"""

from __future__ import annotations

import http.client
import threading
import time
from collections import Counter
from types import TracebackType


class Load:
    """Client threads sending requests until stopped, counting the outcomes.

    Used as a context manager around the operations under test: entering
    starts the clients and returns once the first request has been
    answered, so the operations really happen under load, and leaving
    stops them. `check` then asserts that every request succeeded.
    """

    def __init__(
        self,
        port: int,
        path: str = "/",
        clients: int = 4,
        headers: dict[str, str] | None = None,
    ) -> None:
        self.port = port
        self.path = path
        self.headers = headers or {}
        self.statuses: Counter[int] = Counter()
        self.errors: Counter[str] = Counter()
        self.slowest = 0.0
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._threads = [threading.Thread(target=self._client, daemon=True) for _ in range(clients)]

    def __enter__(self) -> Load:
        for thread in self._threads:
            thread.start()

        # Wait for the first answer, so that what follows happens under
        # load rather than before the clients got going.

        deadline = time.monotonic() + 30

        while self.total == 0 and time.monotonic() < deadline:
            time.sleep(0.01)

        assert self.total > 0, "the load did not get a first answer"
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self.stop()

    def stop(self) -> None:
        """Stop the clients and wait for them."""

        self._stop.set()

        for thread in self._threads:
            if thread.is_alive():
                thread.join()

    @property
    def total(self) -> int:
        """Requests sent so far, answered or not."""

        with self._lock:
            return sum(self.statuses.values()) + sum(self.errors.values())

    @property
    def failures(self) -> dict[str, int]:
        """What did not come back as 200, by status or by error class."""

        with self._lock:
            failed = {
                f"status {status}": count
                for status, count in sorted(self.statuses.items())
                if status != 200
            }
            failed.update({f"error {name}": count for name, count in sorted(self.errors.items())})

        return failed

    def summary(self) -> str:
        """One line with the counts, for an assertion message or a log."""

        with self._lock:
            slowest = self.slowest

        outcome = (
            "all 200"
            if not self.failures
            else ", ".join(f"{count} {what}" for what, count in self.failures.items())
        )
        return f"{self.total} requests, {outcome}, slowest {slowest:.3f}s"

    def check(self, during: str, at_least: int = 50) -> None:
        """Assert that every request succeeded and that there were enough of them."""

        self.stop()
        assert self.total >= at_least, f"only {self.total} requests were made during {during}"
        assert not self.failures, f"failed requests during {during}: {self.summary()}"

    def _connect(self) -> http.client.HTTPConnection:
        return http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)

    def _record(self, status: int | None, error: str | None, elapsed: float) -> None:
        with self._lock:
            if status is not None:
                self.statuses[status] += 1

            if error is not None:
                self.errors[error] += 1

            self.slowest = max(self.slowest, elapsed)

    def _client(self) -> None:
        connection = self._connect()

        try:
            while not self._stop.is_set():
                started = time.monotonic()

                try:
                    connection.request("GET", self.path, headers=self.headers)
                    response = connection.getresponse()
                    response.read()
                    self._record(response.status, None, time.monotonic() - started)
                except (OSError, http.client.HTTPException) as error:
                    self._record(None, type(error).__name__, time.monotonic() - started)
                    connection.close()
                    connection = self._connect()

                time.sleep(0.001)
        finally:
            connection.close()
