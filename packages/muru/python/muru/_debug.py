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
"""

from __future__ import annotations

import itertools
import json
import os
import pdb
import queue
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
_sessions: dict[int, Session] = {}
_waiting: list[Waiting] = []


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


class Waiting:
    """A thread stopped at breakpoint(), waiting for a session."""

    def __init__(self, frame: FrameType) -> None:
        self.frame = frame
        self.where = f"{frame.f_code.co_filename}:{frame.f_lineno}"
        self.thread = threading.get_ident()
        self.ready = threading.Event()
        self.session: Session | None = None


def breakpoint_hook(*args: Any, timeout: float = BREAKPOINT_TIMEOUT, **kwargs: Any) -> None:
    """Wait at the caller's line for muructl debug attach, then debug it there."""

    from muru import _native

    frame = sys._getframe(1)
    waiting = Waiting(frame)

    with _lock:
        _waiting.append(waiting)

    _native.suspend_timeouts()
    index = _worker["index"]
    event(
        "debug.breakpoint",
        "warning",
        f"breakpoint() at {waiting.where} waits {timeout:g} s for muructl debug attach "
        f"--worker {index}",
    )

    waiting.ready.wait(timeout)

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

    Debugger(session).set_trace(frame)


def _shell(session: Session) -> None:
    """Run the session on a thread of its own, for a worker with no breakpoint waiting."""

    debugger = Debugger(session)
    debugger.set_trace(sys._getframe())
    session.finish()


def waiting() -> dict[str, Any]:
    """List the breakpoints waiting for an attach, oldest first."""

    with _lock:
        return {"waiting": [{"where": entry.where, "thread": entry.thread} for entry in _waiting]}


def attach() -> dict[str, Any]:
    """Open a session: on the oldest breakpoint waiting, else on a thread of its own."""

    session = Session()

    with _lock:
        waiting = _waiting.pop(0) if _waiting else None
        _sessions[session.id] = session

        if waiting is not None:
            session.where = waiting.where
            waiting.session = session

    if waiting is not None:
        waiting.ready.set()
    else:
        session.where = "a thread of its own"
        thread = threading.Thread(target=_shell, args=(session,), name="muru-debug", daemon=True)
        thread.start()

    return {"session": session.id, "where": session.where, "output": session.take(2.0)}


def exchange(session_id: int, text: str, wait: float) -> dict[str, Any]:
    """Pass the operator's lines to a session and return what pdb wrote."""

    session = _sessions.get(session_id)

    if session is None:
        raise ValueError(f"no debugger session {session_id} in this worker")

    if text:
        session.feed(text)

    output = session.take(wait)

    if session.ended and not session.output:
        with _lock:
            _sessions.pop(session_id, None)

    return {"output": output, "ended": session.ended}


def detach(session_id: int) -> dict[str, Any]:
    """End a session, letting the debugged code go on."""

    session = _sessions.get(session_id)

    if session is None:
        return {"output": "", "ended": True}

    if not session.ended:
        session.let_go()
        session.feed("continue")

    return exchange(session_id, "", 2.0)
