"""Debugger attach (design specification section 11.7).

``muructl debug attach`` opens a pdb session in a worker, carried as text
through the daemon, the host and the worker's control thread: each
operator line goes in with one request and what pdb wrote since comes back
with its answer. The session takes over a thread stopped at
``breakpoint()``, or, when none is waiting, starts a thread of its own with
the debugger at its first line.

In a worker, ``breakpoint()`` waits for an attach: it records where it
stopped, raises the ``debug.breakpoint`` event, takes its request out of
the timeouts, and gives up after ``timeout`` seconds (60 by default) and
carries on.

With ``dap`` the session is a relay instead: the bytes of the Debug
Adapter Protocol, base64 in each exchange, between the operator's IDE and
the adapter debugpy starts for the worker on a loopback port. A waiting
``breakpoint()`` then stops at its caller's next line once the IDE has
finished configuring.
"""

from __future__ import annotations

import base64
import itertools
import json
import os
import pdb
import queue
import socket
import sys
import threading
import time
from types import FrameType
from typing import Any

__all__ = ["attach", "breakpoint_hook", "detach", "exchange", "install", "waiting"]

# How long breakpoint() waits for an attach, and how long a session may
# last in all and without a line from the operator.

BREAKPOINT_TIMEOUT = 60.0
MAX_SESSION = 3600.0
IDLE_LIMIT = 600.0

# What breakpoint() needs from the worker: its index and the descriptor
# its events go out on.

_worker = {"index": 0, "report": -1}
_ids = itertools.count(1)
_lock = threading.Lock()
_sessions: dict[int, Session | Relay] = {}
_waiting: list[Waiting] = []

# Where debugpy's adapter listens, once a DAP session has started it.

_adapter: dict[str, tuple[str, int]] = {}


def install(index: int, report: int | None) -> None:
    """Make breakpoint() wait for an attach in this worker, unless PYTHONBREAKPOINT is set."""

    _worker["index"] = index
    _worker["report"] = -1 if report is None else report

    if not os.environ.get("PYTHONBREAKPOINT"):
        sys.breakpointhook = breakpoint_hook


def event(kind: str, level: str, message: str) -> None:
    """Raise an event through the host, as the runtime does."""

    report = _worker["report"]

    if report < 0:
        print(f"muru: {message}", file=sys.stderr)
        return

    line = json.dumps({"event": kind, "level": level, "message": message}) + "\n"

    try:
        os.write(report, line.encode())
    except OSError:
        print(f"muru: {message}", file=sys.stderr)


class Session:
    """One operator's pdb session: lines in, pdb's output out."""

    def __init__(self) -> None:
        self.id = next(_ids)
        self.lines: queue.Queue[str] = queue.Queue()
        self.output: list[str] = []
        self.changed = threading.Condition()
        self.ended = False
        self.started = time.monotonic()
        self.last_line = self.started
        self.where = ""
        self.debugger: Debugger | None = None

    # pdb reads and writes through these.

    def readline(self) -> str:
        """Give pdb the operator's next line, or continue once the session is over."""

        while True:
            try:
                return self.lines.get(timeout=1.0)
            except queue.Empty:
                now = time.monotonic()

                if now - self.started > MAX_SESSION or now - self.last_line > IDLE_LIMIT:
                    self.write("\nmuru: the session ran out of time; continuing\n")
                    self.let_go()
                    return "continue\n"

    def write(self, text: str) -> int:
        """Keep what pdb writes for the operator's next exchange."""

        with self.changed:
            self.output.append(text)
            self.changed.notify_all()

        return len(text)

    def flush(self) -> None:
        """Nothing is held back."""

    # The control thread's side.

    def feed(self, text: str) -> None:
        """Pass the operator's lines to pdb."""

        self.last_line = time.monotonic()

        for line in text.splitlines():
            self.lines.put(line + "\n")

    def take(self, wait: float) -> str:
        """Return what pdb wrote, waiting up to `wait` seconds for something."""

        with self.changed:
            if not self.output and not self.ended:
                self.changed.wait(wait)

            text = "".join(self.output)
            self.output.clear()
            return text

    def let_go(self) -> None:
        """Clear the operator's breakpoints, so that continuing ends the session."""

        if self.debugger is not None:
            self.debugger.clear_all_breaks()

    def finish(self) -> None:
        """End the session, as the debugged code has gone on."""

        with self.changed:
            if self.ended:
                return

            self.ended = True
            self.changed.notify_all()

        event("debug.detached", "info", f"the debugger session at {self.where} ended")


class Debugger(pdb.Pdb):
    """pdb on a session, which ends when the debugged code goes on."""

    def __init__(self, session: Session) -> None:
        super().__init__(stdin=session, stdout=session, nosigint=True, readrc=False)  # type: ignore[arg-type]
        self.use_rawinput = False
        self.session = session
        session.debugger = self

    def set_continue(self) -> None:
        """Continue, ending the session once no breakpoint is left."""

        super().set_continue()

        if not self.breaks:
            self.session.finish()

    def set_quit(self) -> None:
        """Quit, which ends the session."""

        super().set_quit()
        self.session.finish()


class Relay:
    """One operator's DAP session: bytes between the IDE and debugpy's adapter."""

    def __init__(self, connection: socket.socket) -> None:
        self.id = next(_ids)
        self.connection = connection
        self.output = bytearray()
        self.changed = threading.Condition()
        self.ended = False
        self.started = time.monotonic()
        self.last_line = self.started
        self.where = ""
        reader = threading.Thread(target=self.read, name="muru-dap", daemon=True)
        _keep_running(reader)
        reader.start()

    def read(self) -> None:
        """Keep what the adapter sends for the operator's next exchange."""

        self.connection.settimeout(1.0)

        while not self.ended:
            now = time.monotonic()

            if now - self.started > MAX_SESSION or now - self.last_line > IDLE_LIMIT:
                break

            try:
                data = self.connection.recv(65536)
            except TimeoutError:
                continue
            except OSError:
                break

            if not data:
                break

            with self.changed:
                self.output.extend(data)
                self.changed.notify_all()

        self.finish()

    def feed(self, data: bytes) -> None:
        """Pass the IDE's bytes to the adapter."""

        self.last_line = time.monotonic()

        if data:
            self.connection.sendall(data)

    def take(self, wait: float) -> bytes:
        """Return what the adapter sent, waiting up to `wait` seconds for something."""

        with self.changed:
            if not self.output and not self.ended:
                self.changed.wait(wait)

            data = bytes(self.output)
            self.output.clear()
            return data

    def finish(self) -> None:
        """End the session; the adapter lets the debugged code go on."""

        with self.changed:
            if self.ended:
                return

            self.ended = True
            self.changed.notify_all()

        self.connection.close()
        event("debug.detached", "info", f"the debugger session at {self.where} ended")


def _keep_running(thread: threading.Thread) -> None:
    """Keep debugpy from tracing a thread or suspending it at a stop.

    The relay and the control thread carry the IDE's messages, so they go
    on while the debugged code is stopped.
    """

    thread.is_pydev_daemon_thread = True  # type: ignore[attr-defined]
    thread.pydev_do_not_trace = True  # type: ignore[attr-defined]


def _adapter_address() -> tuple[str, int]:
    """Start debugpy's adapter for the worker, once, and return where it listens."""

    if "address" not in _adapter:
        os.environ.setdefault("PYDEVD_DISABLE_FILE_VALIDATION", "1")

        try:
            import debugpy
        except ImportError:
            raise RuntimeError("debugpy is not installed in the release's environment") from None

        _adapter["address"] = debugpy.listen(("127.0.0.1", 0))

    return _adapter["address"]


class Waiting:
    """A thread stopped at breakpoint(), waiting for a session."""

    def __init__(self, frame: FrameType) -> None:
        self.frame = frame
        self.where = f"{frame.f_code.co_filename}:{frame.f_lineno}"
        self.thread = threading.get_ident()
        self.ready = threading.Event()
        self.session: Session | Relay | None = None


def breakpoint_hook(*args: Any, timeout: float = BREAKPOINT_TIMEOUT, **kwargs: Any) -> None:
    """Wait at the caller's line for muructl debug attach, then debug it there."""

    from muru import _native

    frame = sys._getframe(1)
    waiting = Waiting(frame)

    with _lock:
        _waiting.append(waiting)

    _native.suspend_timeouts()
    index = _worker["index"]

    # The line names the command that works as it is: the application
    # as the daemon or the run names it, and this worker.

    from muru import context

    event(
        "debug.breakpoint",
        "warning",
        f"breakpoint() at {waiting.where} waits {timeout:g} s for muructl debug attach "
        f"{context().app} --worker {index}",
    )

    # Waiting in a coroutine holds the event loop, so the wait beats for
    # it, and the loop stall timeout does not replace the worker.

    deadline = time.monotonic() + timeout

    while not waiting.ready.wait(min(1.0, max(0.0, deadline - time.monotonic()))):
        _native.loop_beat()

        if time.monotonic() >= deadline:
            break

    # An attach takes the waiting entry and sets its session under the
    # lock, so either it has both or this gives up.

    with _lock:
        if waiting in _waiting:
            _waiting.remove(waiting)

        session = waiting.session

    if session is None:
        event(
            "debug.breakpoint_expired",
            "info",
            f"nobody attached to breakpoint() at {waiting.where} in {timeout:g} s; carrying on",
        )
        return

    if isinstance(session, Relay):
        _stop_for_ide(session, frame)
    else:
        Debugger(session).set_trace(frame)


def _stop_for_ide(relay: Relay, frame: FrameType) -> None:
    """Stop at the frame's next line once the IDE has finished configuring."""

    import debugpy
    from debugpy.server.api import _settrace

    while not debugpy.is_client_connected():
        if relay.ended:
            return

        time.sleep(0.1)

    # The runtime's request threads are not threading's, so debugpy
    # traces one only once told to.

    debugpy.debug_this_thread()

    # debugpy.breakpoint() stops in its own caller, this function;
    # settrace is what it calls, and takes the frame to stop in.

    _settrace(  # type: ignore[no-untyped-call]
        suspend=True,
        trace_only_current_thread=True,
        patch_multiprocessing=False,
        stop_at_frame=frame,
    )


def _shell(session: Session) -> None:
    """Run the session on a thread of its own, for a worker with no breakpoint waiting."""

    debugger = Debugger(session)
    debugger.set_trace(sys._getframe())
    session.finish()


def waiting() -> dict[str, Any]:
    """List the breakpoints waiting for an attach, oldest first."""

    with _lock:
        return {"waiting": [{"where": entry.where, "thread": entry.thread} for entry in _waiting]}


def attach(dap: bool = False) -> dict[str, Any]:
    """Open a session: on the oldest breakpoint waiting, else on a thread of its own.

    With `dap` the session relays the Debug Adapter Protocol to debugpy's
    adapter, which reaches every thread itself.
    """

    session: Session | Relay

    if dap:
        _keep_running(threading.current_thread())
        session = Relay(socket.create_connection(_adapter_address(), timeout=10))
    else:
        session = Session()

    with _lock:
        waiting = _waiting.pop(0) if _waiting else None
        _sessions[session.id] = session

        if waiting is not None:
            session.where = waiting.where
            waiting.session = session

    if waiting is not None:
        waiting.ready.set()
    elif isinstance(session, Relay):
        session.where = "the worker's threads"
    else:
        session.where = "a thread of its own"
        thread = threading.Thread(target=_shell, args=(session,), name="muru-debug", daemon=True)
        thread.start()

    if isinstance(session, Relay):
        return {"session": session.id, "where": session.where, "dap": True}

    return {"session": session.id, "where": session.where, "output": session.take(2.0)}


def exchange(session_id: int, text: str, wait: float, data: str = "") -> dict[str, Any]:
    """Pass the operator's lines to a session and return what pdb wrote.

    A DAP session takes and returns base64 `data` instead.
    """

    session = _sessions.get(session_id)

    if session is None:
        raise ValueError(f"no debugger session {session_id} in this worker")

    if isinstance(session, Relay):
        session.feed(base64.b64decode(data))
        sent = session.take(wait)
        answer = {"data": base64.b64encode(sent).decode(), "ended": session.ended}
    else:
        if text:
            session.feed(text)

        answer = {"output": session.take(wait), "ended": session.ended}

    if session.ended and not session.output:
        with _lock:
            _sessions.pop(session_id, None)

    return answer


def detach(session_id: int) -> dict[str, Any]:
    """End a session, letting the debugged code go on."""

    session = _sessions.get(session_id)

    if session is None:
        return {"output": "", "ended": True}

    if isinstance(session, Relay):
        session.finish()
        return exchange(session_id, "", 0.0)

    if not session.ended:
        session.let_go()
        session.feed("continue")

    # pdb may write once more before the debugged code goes on and the
    # session ends, so the wait is for the end itself, not the next output.

    deadline = time.monotonic() + 2.0
    output = ""

    while True:
        answer = exchange(session_id, "", max(0.0, deadline - time.monotonic()))
        output += answer["output"]

        if answer["ended"] or time.monotonic() >= deadline:
            answer["output"] = output
            return answer
