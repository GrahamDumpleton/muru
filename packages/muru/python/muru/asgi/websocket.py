"""A WebSocket as callbacks on threads (design specification section 6.14).

``Handler`` is an ordinary ASGI application. It accepts the socket and
calls the handler object's ``on_connect(ws)``, ``on_receive(ws, message)``
for each message, a str for text and bytes for binary, and
``on_disconnect(ws, code, reason)``, each on an application thread, one at
a time per socket and in order. Every call for a socket runs in one
context, created at accept, so a context variable set in ``on_connect``
is visible in later calls whichever thread runs them. A WSGI view hands
an upgrade to it; it runs as well under any ASGI server.
"""

from __future__ import annotations

import asyncio
import concurrent.futures
import contextvars
import sys
import threading
import traceback
from collections.abc import Awaitable, Callable, Iterable, MutableMapping
from typing import Any

__all__ = ["ConnectionClosed", "Handler", "Socket"]

Send = Callable[[dict[str, Any]], Awaitable[None]]
Receive = Callable[[], Awaitable[dict[str, Any]]]

# The close code an exception from a callback ends the socket with.

INTERNAL_ERROR = 1011


class ConnectionClosed(Exception):
    """The socket is closed: the peer went away, or it was closed here."""

    def __init__(self, code: int, reason: str = "") -> None:
        super().__init__(f"the WebSocket is closed ({code}{': ' + reason if reason else ''})")
        self.code = code
        self.reason = reason


class Socket:
    """One accepted WebSocket, as the callbacks see it.

    ``send`` and ``close`` may be called from any thread, so a callback can
    send to other sockets, which is how a broadcast is written.
    """

    def __init__(
        self, send: Send, loop: asyncio.AbstractEventLoop, subprotocol: str | None
    ) -> None:
        self._send = send
        self._loop = loop
        self._closed: tuple[int, str] | None = None
        self._lock = threading.Lock()
        self.subprotocol = subprotocol
        """The subprotocol the socket was accepted with, or None."""

    @property
    def closed(self) -> bool:
        """Whether the socket is closed."""

        return self._closed is not None

    def _mark_closed(self, code: int, reason: str) -> None:
        with self._lock:
            if self._closed is None:
                self._closed = (code, reason)

    def _run(self, message: dict[str, Any]) -> None:
        """Send a message on the loop: wait for it from a thread, queue it from the loop."""

        coroutine = self._send(message)

        try:
            running = asyncio.get_running_loop()
        except RuntimeError:
            running = None

        if running is self._loop:
            self._loop.create_task(coroutine)  # type: ignore[arg-type]
            return

        asyncio.run_coroutine_threadsafe(coroutine, self._loop).result()  # type: ignore[arg-type]

    def send(self, data: str | bytes) -> None:
        """Send a text message for a str, a binary one for bytes.

        Waits only while the client is slow to take what was sent before.
        Raises ConnectionClosed once the socket is closed.
        """

        closed = self._closed

        if closed is not None:
            raise ConnectionClosed(*closed)

        if isinstance(data, str):
            self._run({"type": "websocket.send", "text": data})
        else:
            self._run({"type": "websocket.send", "bytes": bytes(data)})

    def close(self, code: int = 1000, reason: str = "") -> None:
        """Close the socket with a code and reason; closing it again does nothing."""

        with self._lock:
            if self._closed is not None:
                return

            self._closed = (code, reason)

        self._run({"type": "websocket.close", "code": code, "reason": reason})


class _WorkerPool(concurrent.futures.Executor):
    """The worker's application thread pool, as an executor."""

    def submit(
        self, fn: Callable[..., Any], /, *args: Any, **kwargs: Any
    ) -> concurrent.futures.Future[Any]:
        from muru import _native

        future: concurrent.futures.Future[Any] = concurrent.futures.Future()

        def job() -> None:
            if not future.set_running_or_notify_cancel():
                return

            try:
                future.set_result(fn(*args, **kwargs))
            except BaseException as error:
                future.set_exception(error)

        if not _native.submit(job):
            raise RuntimeError("the worker's thread pool is not running")

        return future


_own_pool: concurrent.futures.ThreadPoolExecutor | None = None
_own_pool_lock = threading.Lock()


def _default_executor() -> concurrent.futures.Executor:
    """Return the worker's thread pool under Muru, else a pool of the adapter's own."""

    global _own_pool

    from muru import _asgi

    if _asgi.current_loop() is not None:
        return _WorkerPool()

    with _own_pool_lock:
        if _own_pool is None:
            _own_pool = concurrent.futures.ThreadPoolExecutor(thread_name_prefix="muru-websocket")

        return _own_pool


def _encode(headers: Iterable[tuple[str | bytes, str | bytes]]) -> list[tuple[bytes, bytes]]:
    def raw(value: str | bytes) -> bytes:
        return value.encode("latin-1") if isinstance(value, str) else value

    return [(raw(name), raw(value)) for name, value in headers]


class Handler:
    """An ASGI application that runs a WebSocket's callbacks on threads.

    ``handler`` provides any of ``on_connect``, ``on_receive`` and
    ``on_disconnect``. The socket is accepted with ``subprotocol`` when
    the client offered it, and with the extra response ``headers``.
    ``executor`` runs the callbacks; by default the worker's application
    threads under Muru, and a thread pool of the adapter's own under any
    other server. An exception from a callback closes the socket with
    1011.
    """

    def __init__(
        self,
        handler: Any,
        subprotocol: str | None = None,
        headers: Iterable[tuple[str | bytes, str | bytes]] | None = None,
        executor: concurrent.futures.Executor | None = None,
    ) -> None:
        self.handler = handler
        self.subprotocol = subprotocol
        self.headers = _encode(headers or ())
        self.executor = executor

    async def __call__(self, scope: MutableMapping[str, Any], receive: Receive, send: Send) -> None:
        """Accept the WebSocket and run its callbacks until it closes."""

        if scope["type"] != "websocket":
            raise RuntimeError(
                f"muru.asgi.websocket.Handler serves WebSockets, not {scope['type']}"
            )

        loop = asyncio.get_running_loop()
        executor = self.executor or _default_executor()

        connect = await receive()

        if connect["type"] != "websocket.connect":
            return

        # The subprotocol is chosen only when the client offered it.

        offered = scope.get("subprotocols") or []
        subprotocol = self.subprotocol if self.subprotocol in offered else None
        accept: dict[str, Any] = {"type": "websocket.accept", "headers": self.headers}

        if subprotocol is not None:
            accept["subprotocol"] = subprotocol

        await send(accept)
        socket = Socket(send, loop, subprotocol)

        # One context for every call on this socket, so values set in
        # on_connect are seen by the calls after it.

        context = contextvars.copy_context()

        async def call(name: str, *args: Any) -> bool:
            method = getattr(self.handler, name, None)

            if method is None:
                return True

            try:
                await loop.run_in_executor(executor, context.run, method, socket, *args)
            except Exception:
                sys.stderr.write(f"muru: WebSocket {name} raised:\n{traceback.format_exc()}")
                sys.stderr.flush()
                return False

            return True

        if await call("on_connect"):
            while True:
                message = await receive()

                if message["type"] == "websocket.disconnect":
                    code = int(message.get("code", 1005))
                    reason = str(message.get("reason") or "")
                    socket._mark_closed(code, reason)
                    await call("on_disconnect", code, reason)
                    return

                data = message.get("text")

                if data is None:
                    data = message.get("bytes", b"")

                if not await call("on_receive", data):
                    break

        # A callback raised: close with 1011 and tell the handler, unless
        # the socket was already closed.

        if not socket.closed:
            socket._mark_closed(INTERNAL_ERROR, "internal error")
            await send(
                {"type": "websocket.close", "code": INTERNAL_ERROR, "reason": "internal error"}
            )

        await call("on_disconnect", INTERNAL_ERROR, "internal error")
