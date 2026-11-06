"""The worker's event loop and the ASGI lifespan (design specification section 6.6).

One asyncio loop runs on its own thread, started after the fork. The native
extension schedules a task on it for each request, with the scope it built
and a channel whose ``receive`` and ``send`` the application calls; what
happens on the loop is here. The lifespan protocol runs once per worker,
before it accepts requests and again when it drains.
"""

from __future__ import annotations

import asyncio
import math
import sys
import threading
import traceback
from collections.abc import Awaitable, Callable, Coroutine, MutableMapping
from concurrent.futures import Future, ThreadPoolExecutor
from typing import Any

from muru._context import _request_id

__all__ = [
    "Lifespan",
    "LifespanFailed",
    "Loop",
    "current_loop",
    "heartbeat",
    "register",
    "runner",
    "start_handoff",
    "start_registered",
    "stop_registered",
]

ASGIApp = Callable[[MutableMapping[str, Any], Any, Any], Awaitable[None]]

STARTUP_TIMEOUT = 60.0
SHUTDOWN_TIMEOUT = 10.0

# Section 7.6: how often the loop tells the native watchdog it still runs.

HEARTBEAT_INTERVAL = 0.5

# Section 6.6: the pools synchronous code runs on, by their place in the
# runtime's list: the loop's own, anyio's worker threads and asgiref's
# one shared thread for thread-sensitive calls.

POOL_LOOP = 0
POOL_ANYIO = 1
POOL_ASGIREF = 2

# The thread limit anyio starts with, which the worker's replaces.

ANYIO_DEFAULT_THREADS = 40


def heartbeat(loop: asyncio.AbstractEventLoop) -> None:
    """Make the loop beat for the native watchdog from now until it stops.

    Each beat also tells the runtime how busy the pools for synchronous
    code are that it cannot count by itself (section 6.6).
    """

    from muru import _native

    def beat() -> None:
        _native.loop_beat()

        if _current is not None and _current.loop is loop:
            _current.sample_sync_pools()

        loop.call_later(HEARTBEAT_INTERVAL, beat)

    loop.call_soon_threadsafe(beat)


def _name_thread() -> None:
    """Tell the runtime the calling thread's name.

    A reader from outside the worker (section 11.7) sees a thread by its
    identifier alone, and the runtime's bookkeeping is where it finds
    the names.
    """

    from muru import _native

    _native.name_thread(threading.current_thread().name)


class SyncPool(ThreadPoolExecutor):
    """A pool the event loop runs synchronous code on (section 6.6).

    Each call is noted with the runtime while a thread of the pool runs
    it, with the request it was made in, so the pool's busy threads and
    the request each one serves can be seen from outside (section 11.7).
    `pool` says which pool it is to the runtime: the loop's own, or the
    one thread asgiref keeps for thread-sensitive calls.
    """

    def __init__(self, max_workers: int, thread_name_prefix: str, pool: int = POOL_LOOP) -> None:
        super().__init__(
            max_workers=max_workers,
            thread_name_prefix=thread_name_prefix,
            initializer=_name_thread,
        )
        self.pool = pool

    def submit(self, fn: Callable[..., Any], /, *args: Any, **kwargs: Any) -> Future[Any]:
        """Queue a call, as the executor does, noting it while it runs."""

        # The request is read here, in the context of the task making
        # the call: the thread that runs it has a context of its own
        # unless the caller copied one.

        from muru import _native

        request = _request_id.get()
        pool = self.pool

        def held(*args: Any, **kwargs: Any) -> Any:
            _native.sync_entered(request, pool)

            try:
                return fn(*args, **kwargs)
            finally:
                _native.sync_left(request, pool)

        return super().submit(held, *args, **kwargs)


class Loop:
    """The worker's event loop, running on a thread of its own."""

    def __init__(self, sync_threads: int = 8) -> None:
        self.loop = asyncio.new_event_loop()
        self.thread = threading.Thread(target=self._run, name="muru-loop", daemon=True)
        self.started = threading.Event()

        # Section 6.6: asyncio.to_thread, and anything else that uses the
        # loop's default executor, runs on a bounded pool of the loop's
        # own, separate from any WSGI threads.

        self.sync_threads = max(1, sync_threads)
        executor = SyncPool(max_workers=self.sync_threads, thread_name_prefix="muru-sync")
        self.loop.set_default_executor(executor)

        # The other places synchronous code runs, once adopted: anyio's
        # thread limiter, and whether asgiref's shared thread is noted.

        self.anyio_limiter: Any = None
        self.asgiref = False

    def adopt_sync_pools(self) -> None:
        """Bring the other places synchronous code runs under the worker (section 6.6).

        anyio, which Starlette and FastAPI run synchronous endpoints
        through, has worker threads of its own, 40 by default: its limit
        becomes the worker's sync threads. asgiref runs thread-sensitive
        calls outside a request's own context on one shared thread, which
        is replaced by one whose calls are noted. Called once the loop
        runs and the applications are loaded, before any lifespan
        startup, so a limit the application sets there stands.
        """

        self._adopt_asgiref()
        done = threading.Event()

        def adopt() -> None:
            try:
                self._adopt_anyio()
            finally:
                done.set()

        self.loop.call_soon_threadsafe(adopt)
        done.wait(STARTUP_TIMEOUT)

    def _adopt_asgiref(self) -> None:
        owner = getattr(sys.modules.get("asgiref.sync"), "SyncToAsync", None)
        executor = getattr(owner, "single_thread_executor", None)

        # An executor that is not the one expected, or one whose thread
        # has run a call already, is left as it is: thread-sensitive code
        # must keep the thread it has used.

        if (
            owner is None
            or not isinstance(executor, ThreadPoolExecutor)
            or isinstance(executor, SyncPool)
            or getattr(executor, "_threads", None)
        ):
            return

        owner.single_thread_executor = SyncPool(
            max_workers=1, thread_name_prefix="muru-sync-asgiref", pool=POOL_ASGIREF
        )
        self.asgiref = True

    def _adopt_anyio(self) -> None:
        """Set anyio's thread limit to the worker's; on the loop's thread."""

        if self.anyio_limiter is not None or "anyio" not in sys.modules:
            return

        try:
            from anyio import to_thread

            limiter = to_thread.current_default_thread_limiter()

            if limiter.total_tokens == ANYIO_DEFAULT_THREADS:
                limiter.total_tokens = self.sync_threads
        except Exception:
            # An anyio this does not know is left as it is.

            return

        self.anyio_limiter = limiter

    def sample_sync_pools(self) -> None:
        """Tell the runtime how busy anyio's threads are; on the loop's thread.

        anyio imported since the worker started is adopted here, and
        asgiref's shared thread, whose calls the runtime is told of one
        by one, is declared.
        """

        from muru import _native

        self._adopt_anyio()
        limiter = self.anyio_limiter

        if limiter is not None:
            total = limiter.total_tokens
            threads = 0 if total == math.inf else int(total)
            _native.sync_pool(POOL_ANYIO, threads, int(limiter.borrowed_tokens))

        if self.asgiref:
            _native.sync_pool(POOL_ASGIREF, 1)

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

        global _current

        self.thread.start()
        self.started.wait()
        _current = self

    def stop(self, timeout: float = SHUTDOWN_TIMEOUT) -> None:
        """Stop the loop and wait for its thread, if it is running."""

        global _current

        if _current is self:
            _current = None

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


# Section 6.9: the worker's running loop, for muru.loop().

_current: Loop | None = None


def current_loop() -> asyncio.AbstractEventLoop | None:
    """Return the worker's running event loop, or None."""

    return _current.loop if _current is not None else None


# Section 6.14: applications whose lifespan runs because requests are
# handed to them. Those registered before the loop starts wait for it;
# a started one is kept by the application's identity, with the
# application itself, so the identity stays valid.

_pending: list[ASGIApp] = []
_registered: dict[int, Lifespan] = {}


def describe(app: Any) -> str:
    """Name an application for the log: its module and qualified name."""

    kind = app if callable(app) and hasattr(app, "__qualname__") else type(app)
    return f"{getattr(kind, '__module__', '?')}:{getattr(kind, '__qualname__', repr(app))}"


def register(app: ASGIApp) -> None:
    """Run ``app``'s lifespan in this worker: now, or once the loop starts."""

    if id(app) in _registered or any(waiting is app for waiting in _pending):
        return

    if _current is None:
        _pending.append(app)
        return

    if threading.current_thread() is _current.thread:
        raise RuntimeError(
            "muru.asgi.register() cannot run on the event loop; call it from a post_fork hook"
        )

    lifespan = Lifespan(app, _current, describe(app))
    lifespan.startup()
    _registered[id(app)] = lifespan


def start_registered(loop: Loop | None) -> None:
    """Run the startup of every application registered before the loop started.

    Raises LifespanFailed when one fails, and ValueError when there is
    no loop to run them on.
    """

    if not _pending:
        return

    if loop is None:
        names = ", ".join(describe(app) for app in _pending)
        raise ValueError(
            f"muru.asgi.register() was given {names}, but this worker has no event loop; "
            'set handoff = "asgi" or serve an ASGI mount'
        )

    while _pending:
        app = _pending.pop(0)
        lifespan = Lifespan(app, loop, describe(app))
        lifespan.startup()
        _registered[id(app)] = lifespan


def stop_registered() -> None:
    """Run the shutdown of every registered application, most recent first."""

    for lifespan in reversed(list(_registered.values())):
        lifespan.shutdown()

    _registered.clear()


async def _serve(app: ASGIApp, scope: MutableMapping[str, Any], channel: Any) -> None:
    """Run one request's application against its channel, and end it."""

    extensions = scope.get("extensions") or {}
    _request_id.set(extensions.get("muru", {}).get("request_id"))

    async def send(message: dict[str, Any]) -> None:
        # The channel only hands back a future when the writer is behind.

        waiter = channel.send(message)
        if waiter is not None:
            await waiter

    try:
        await app(scope, channel.receive, send)
    except BaseException as error:
        # A failed request never ends the worker. A task the soft
        # timeout cancelled (section 7.6) fails the response as a
        # timeout, and the traceback shows where it waited.

        detail = "".join(traceback.format_exception(error))
        sys.stderr.write(detail)
        sys.stderr.flush()
        channel.fail(detail)
    else:
        channel.finish()


def runner(app: ASGIApp, loop: Loop, state: dict[str, Any]) -> Callable[[Any, Any], None]:
    """Return the callable the extension schedules on the loop for each request.

    It receives the scope and the channel, gives the scope a copy of the
    lifespan state, and starts the application as a task of its own, so
    each request runs in its own context as asyncio tasks do.
    """

    event_loop = loop.loop

    def run(scope: MutableMapping[str, Any], channel: Any) -> None:
        # The state is filled in place: a handoff to WSGI holds the same
        # dictionary for environ["muru.state"].

        scope["state"].update(state)
        channel.started(event_loop.create_task(_serve(app, scope, channel)))

    return run


def start_handoff(app: ASGIApp, scope: MutableMapping[str, Any], channel: Any) -> None:
    """Start a WSGI request handed to ``app`` (section 6.14).

    Scheduled on the loop by the extension in a copy of the WSGI
    request's context, which the task inherits. A registered
    application's lifespan state is copied into the scope, under what
    the WSGI code put in its state.
    """

    lifespan = _registered.get(id(app))

    if lifespan is not None and lifespan.app is app:
        state = dict(lifespan.state)
        state.update(scope["state"])
        scope["state"] = state

    loop = asyncio.get_running_loop()
    channel.started(loop.create_task(_serve(app, scope, channel)))
