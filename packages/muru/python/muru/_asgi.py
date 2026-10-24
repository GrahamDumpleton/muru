"""The worker's event loop and the ASGI lifespan (design specification section 6.6).

One asyncio loop runs on its own thread, started after the fork. The native
extension schedules a task on it for each request, with the scope it built
and a channel whose ``receive`` and ``send`` the application calls; what
happens on the loop is here. The lifespan protocol runs once per worker,
before it accepts requests and again when it drains.
"""

from __future__ import annotations

import asyncio
import sys
import threading
import traceback
from collections.abc import Awaitable, Callable, Coroutine, MutableMapping
from concurrent.futures import ThreadPoolExecutor
from typing import Any

__all__ = ["Lifespan", "LifespanFailed", "Loop", "runner"]

ASGIApp = Callable[[MutableMapping[str, Any], Any, Any], Awaitable[None]]

STARTUP_TIMEOUT = 60.0
SHUTDOWN_TIMEOUT = 10.0


class Loop:
    """The worker's event loop, running on a thread of its own."""

    def __init__(self, sync_threads: int = 8) -> None:
        self.loop = asyncio.new_event_loop()
        self.thread = threading.Thread(target=self._run, name="muru-loop", daemon=True)
        self.started = threading.Event()

        # Section 6.6: sync_to_async and asyncio.to_thread run on a bounded
        # pool of their own, separate from any WSGI threads.

        executor = ThreadPoolExecutor(
            max_workers=max(1, sync_threads), thread_name_prefix="muru-sync"
        )
        self.loop.set_default_executor(executor)

    def _run(self) -> None:
        asyncio.set_event_loop(self.loop)
        self.loop.call_soon(self.started.set)

        try:
            self.loop.run_forever()
        finally:
            # Finish what the loop still owns before it closes.

            try:
                self.loop.run_until_complete(self.loop.shutdown_asyncgens())
                self.loop.run_until_complete(self.loop.shutdown_default_executor())
            finally:
                self.loop.close()

    def start(self) -> None:
        """Start the thread and return once the loop is running."""

        self.thread.start()
        self.started.wait()

    def stop(self, timeout: float = SHUTDOWN_TIMEOUT) -> None:
        """Stop the loop and wait for its thread, if it is running."""

        if not self.thread.is_alive():
            return

        self.loop.call_soon_threadsafe(self.loop.stop)
        self.thread.join(timeout)

    def run(self, coroutine: Coroutine[Any, Any, Any], timeout: float | None = None) -> Any:
        """Run a coroutine on the loop from another thread and wait for its result."""

        future = asyncio.run_coroutine_threadsafe(coroutine, self.loop)
        return future.result(timeout)


class LifespanFailed(Exception):
    """The application reported ``lifespan.startup.failed`` or raised during startup."""


class Lifespan:
    """The lifespan protocol for one application on the worker's loop.

    ``startup`` sends ``lifespan.startup`` and waits for the application's
    answer. An application that raises before answering does not support
    lifespan, which is allowed; one that answers ``lifespan.startup.failed``
    or raises after answering fails the worker. The ``state`` dictionary
    the application fills during startup is copied into the scope of every
    request.
    """

    def __init__(self, app: ASGIApp, loop: Loop, name: str = "the application") -> None:
        self.app = app
        self.loop = loop
        self.name = name
        self.state: dict[str, Any] = {}
        self.supported = True

        # Created on the loop when startup runs.

        self._queue: asyncio.Queue[dict[str, Any]] | None = None
        self._startup: asyncio.Future[str] | None = None
        self._shutdown: asyncio.Future[str] | None = None
        self._task: asyncio.Task[None] | None = None

    async def _receive(self) -> dict[str, Any]:
        assert self._queue is not None
        return await self._queue.get()

    async def _send(self, message: dict[str, Any]) -> None:
        kind = message.get("type")
        failure = str(message.get("message", ""))

        if kind in ("lifespan.startup.complete", "lifespan.startup.failed"):
            if self._startup is not None and not self._startup.done():
                self._startup.set_result(failure if kind.endswith("failed") else "")
            if kind.endswith("failed"):
                raise LifespanFailed(failure)
        elif kind in ("lifespan.shutdown.complete", "lifespan.shutdown.failed"):
            if self._shutdown is not None and not self._shutdown.done():
                self._shutdown.set_result(failure if kind.endswith("failed") else "")
        else:
            raise RuntimeError(f"unexpected ASGI message type {kind!r} on the lifespan connection")

    async def _run(self) -> None:
        scope: dict[str, Any] = {
            "type": "lifespan",
            "asgi": {"version": "3.0", "spec_version": "2.0"},
            "state": self.state,
        }

        try:
            await self.app(scope, self._receive, self._send)
        except LifespanFailed:
            raise
        except BaseException as error:
            # Raised before answering startup: the application has no
            # lifespan support, which ASGI allows. Raised later, it is a
            # failure like any other.

            startup = self._startup
            if startup is None or startup.done():
                raise
            self.supported = False
            startup.set_result("")
            if not isinstance(error, Exception):
                raise

    async def _start(self) -> None:
        loop = asyncio.get_running_loop()
        self._queue = asyncio.Queue()
        self._startup = loop.create_future()
        self._shutdown = loop.create_future()
        self._task = loop.create_task(self._run())

        # Ask for startup and wait for the answer, or for the application
        # to give up first.

        await self._queue.put({"type": "lifespan.startup"})
        awaited: set[asyncio.Future[Any]] = {self._startup, self._task}
        await asyncio.wait(awaited, return_when=asyncio.FIRST_COMPLETED)

        # An application still running after answering is the usual case.
        # One that has already ended either reported a failure, raised one,
        # or has no lifespan support and simply returned.

        if not self._task.done():
            return
        error = self._task.exception()

        if isinstance(error, LifespanFailed):
            message = self._startup.result() if self._startup.done() else ""
            raise LifespanFailed(message or f"{self.name} reported lifespan.startup.failed")

        if error is not None:
            raise LifespanFailed(f"{self.name} failed during lifespan startup: {error}")
        if not self._startup.done():
            self.supported = False

    async def _stop(self) -> None:
        if self._task is None or self._task.done() or self._queue is None or self._shutdown is None:
            return

        # Ask for shutdown and give the application a bounded time to finish.

        await self._queue.put({"type": "lifespan.shutdown"})

        try:
            await asyncio.wait_for(asyncio.shield(self._task), SHUTDOWN_TIMEOUT)
        except TimeoutError:
            print(f"muru: {self.name} did not complete lifespan shutdown in time", file=sys.stderr)
            self._task.cancel()
            return

        if self._shutdown.done() and self._shutdown.result():
            message = self._shutdown.result()
            print(
                f"muru: {self.name} reported lifespan.shutdown.failed: {message}", file=sys.stderr
            )

    def startup(self) -> None:
        """Run startup on the loop. Raises LifespanFailed when the application fails."""

        try:
            self.loop.run(self._start(), STARTUP_TIMEOUT)
        except TimeoutError as error:
            raise LifespanFailed(
                f"{self.name} did not complete lifespan startup in time"
            ) from error
        except LifespanFailed:
            raise
        except Exception as error:
            raise LifespanFailed(f"{self.name} failed during lifespan startup: {error}") from error

    def shutdown(self) -> None:
        """Send shutdown on the loop and wait for the application, within limits."""

        try:
            self.loop.run(self._stop(), SHUTDOWN_TIMEOUT + 1)
        except Exception as error:  # shutdown must not fail the worker
            print(f"muru: lifespan shutdown of {self.name} failed: {error}", file=sys.stderr)


def runner(app: ASGIApp, loop: Loop, state: dict[str, Any]) -> Callable[[Any, Any], None]:
    """Return the callable the extension schedules on the loop for each request.

    It receives the scope and the channel, gives the scope a copy of the
    lifespan state, and starts the application as a task of its own, so
    each request runs in its own context as asyncio tasks do.
    """

    event_loop = loop.loop

    async def serve(scope: MutableMapping[str, Any], channel: Any) -> None:
        async def send(message: dict[str, Any]) -> None:
            # The channel only hands back a future when the writer is behind.

            waiter = channel.send(message)
            if waiter is not None:
                await waiter

        try:
            await app(scope, channel.receive, send)
        except BaseException as error:  # a failed request never ends the worker
            detail = "".join(traceback.format_exception(error))
            sys.stderr.write(detail)
            sys.stderr.flush()
            channel.fail(detail)
        else:
            channel.finish()

    def run(scope: MutableMapping[str, Any], channel: Any) -> None:
        scope["state"] = dict(state)
        event_loop.create_task(serve(scope, channel))

    return run
