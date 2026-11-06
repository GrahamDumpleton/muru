"""Reading a worker from outside (specification section 11.7).

PEP 768 lets another process read a running interpreter's frames on
Python 3.14 and later, without the interpreter's cooperation, so the host
can see the stacks of a worker whose threads hold the interpreter or
spin. The host is the reader: it is the workers' parent and runs as their
user, which Linux's default ptrace scope allows. macOS allows it only to
root, so there the host falls back to asking the worker itself, as it
does on older interpreters.

Threads read from outside have identifiers but no names. The worker's
bookkeeping, which the runtime answers without the interpreter, supplies
the names of the threads that tell the runtime theirs and the request
each thread serves. On Linux the identifier is the thread's TID, and the
kernel holds the name Python 3.14 and later give each thread they start,
which names the rest (ADR 0053).
"""

from __future__ import annotations

import ast
import codecs
import functools
import linecache
import os
import sys
import time
from collections import Counter
from typing import Any

# The nodes that have a code object of their own, and so a frame.

_SCOPES = (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef, ast.GeneratorExp)


# Python 3.14's unwinder stops where C code called into Python, so a
# request's stack read from outside loses every frame above a builtin
# such as breakpoint(), and Rust calling the application hides the
# runtime's own; 3.15's walks the whole chain.

WHOLE = sys.version_info >= (3, 15)


# Where Linux shows each process's threads, and so the name each thread
# has given itself.

PROC = "/proc"

# A thread read from outside: its identifier, its frames, root first, and
# the name the operating system holds for it, if it has one.

Thread = tuple[int, list[dict[str, Any]], str | None]


class Unreadable(Exception):
    """The worker's interpreter cannot be read from outside."""


def _unwinder(pid: int) -> Any:
    try:
        import _remote_debugging  # type: ignore[import-not-found]
    except ImportError:
        raise Unreadable("reading from outside needs Python 3.14 or later") from None

    try:
        return _remote_debugging.RemoteUnwinder(pid, all_threads=True)
    except (OSError, RuntimeError) as error:
        raise Unreadable(str(error)) from None


def _comm(path: str) -> str | None:
    """Return the thread name in a `comm` file, or None.

    Linux keeps 15 bytes of a name and may cut a character in two, so a
    piece of one left at the end is dropped, and any other byte that is
    not UTF-8 is replaced. None means the file could not be read, as
    when the thread has ended, or held no name.
    """

    try:
        with open(path, "rb") as file:
            raw = file.read()
    except OSError:
        return None

    # Decoding as if more were to come holds back an unfinished
    # character rather than replacing it.

    decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
    return decoder.decode(raw.rstrip(b"\n"), final=False) or None


class Names:
    """The names Linux holds for one worker's threads, read once each.

    Python 3.14 and later give the kernel a `threading` thread's name as
    the thread starts and when it renames itself, cut to 15 bytes, so a
    thread the runtime has no name for is still named by the TID it is
    read under. The main thread's TID is the worker's pid, and its name
    is the process's, which a thread inherits from the one that started
    it when nothing names it; such a name says nothing and is left out.
    """

    def __init__(self, pid: int, proc: str = PROC) -> None:
        self.pid = pid
        self.task = os.path.join(proc, str(pid), "task")
        self.process = _comm(os.path.join(proc, str(pid), "comm"))
        self.known: dict[int, str | None] = {pid: "MainThread"}

    def get(self, tid: int) -> str | None:
        """Return the thread's name, or None when it has none to give."""

        if tid not in self.known:
            name = _comm(os.path.join(self.task, str(tid), "comm"))
            self.known[tid] = None if name == self.process else name

        return self.known[tid]


def _line(location: Any) -> int:
    """Return a frame's line from an int (3.14) or a location (3.15)."""

    if isinstance(location, int):
        return location

    return int(getattr(location, "lineno", 0) or 0)


def _threads(trace: Any) -> list[tuple[int, list[dict[str, Any]]]]:
    """Return each thread's identifier and frames, root first.

    The stack trace may be of either interpreter version's shape.
    """

    found = []

    for item in trace:
        for thread in getattr(item, "threads", [item]):
            ident = getattr(thread, "thread_id", None)
            frames = getattr(thread, "frame_info", None)

            if ident is None:
                ident, frames = thread[0], thread[1]

            found.append(
                (
                    int(ident),
                    [
                        {"file": frame[0], "line": _line(frame[1]), "function": frame[2]}
                        for frame in reversed(list(frames or []))
                    ],
                )
            )

    return found


@functools.lru_cache(maxsize=256)
def _scopes(file: str, mtime: float) -> list[tuple[int, int, str, int]] | None:
    """Return each scope in a file: its lines, its name and where it starts.

    Where a scope starts is its code object's first line, which for a
    decorated function or class is its first decorator's. None means the
    file cannot be read or parsed.
    """

    try:
        with open(file, "rb") as source:
            tree = ast.parse(source.read(), file)
    except (OSError, SyntaxError, ValueError):
        return None

    found = []

    for node in ast.walk(tree):
        if not isinstance(node, _SCOPES):
            continue

        if isinstance(node, ast.Lambda):
            name = "<lambda>"
        elif isinstance(node, ast.GeneratorExp):
            name = "<genexpr>"
        else:
            name = node.name

        decorators = getattr(node, "decorator_list", [])
        start = min([node.lineno, *(decorator.lineno for decorator in decorators)])
        found.append((node.lineno, node.end_lineno or node.lineno, name, start))

    return found


def first_line(file: str, line: int, function: str) -> int:
    """Return where the function a frame is running starts.

    A frame read from outside gives its file, the line it is running and
    its function's name; the in-process profile labels a function by
    where it starts, so the innermost scope of that name holding the line
    is found in the file's source. The running line is returned when it
    cannot be.
    """

    if function == "<module>":
        return 1

    try:
        scopes = _scopes(file, os.stat(file).st_mtime)
    except OSError:
        return line

    name = function.rsplit(".", 1)[-1]
    holding = [scope for scope in scopes or [] if scope[0] <= line <= scope[1] and scope[2] == name]

    if not holding:
        return line

    return max(holding, key=lambda scope: scope[0])[3]


def parked(frames: list[dict[str, Any]]) -> bool:
    """Say whether a thread is one the runtime parks.

    Such a thread has no Python frames, or waits for work in the
    runtime's own frame (`muru._pool`).
    """

    return not frames or frames[-1]["file"].endswith(os.path.join("muru", "_pool.py"))


def _main_thread(frames: list[dict[str, Any]]) -> bool:
    """Say whether a thread is the worker's main thread.

    It waits in the runtime, called from the host's module, for the
    worker's life. On Linux the main thread is known by its TID, which
    is the worker's pid; this is for a reader whose identifiers are not.
    """

    return bool(frames) and frames[-1]["file"].endswith(os.path.join("muru", "host.py"))


def stacks_result(threads: list[Thread], bookkeeping: dict[str, Any]) -> dict[str, Any]:
    """Report threads read from outside as the in-process stacks do.

    The bookkeeping names the threads the runtime knows and gives the
    request each serves; the operating system's name, where the reader
    found one, names the others.
    """

    active = bookkeeping.get("active", [])
    names = bookkeeping.get("names", {})

    # A thread read from outside is known by its native identifier on
    # Linux, and by its Python one elsewhere.

    serving = {
        key: entry
        for entry in active
        for key in (entry.get("thread"), entry.get("native"))
        if key is not None
    }

    # An ASGI request's code runs on a pool for synchronous code for as
    # long as a call there takes.

    for entry in active:
        for key in (*(entry.get("sync_threads") or ()), *(entry.get("sync_native") or ())):
            serving.setdefault(key, entry)

    found = [
        {
            "ident": ident,
            "name": names.get(str(ident))
            or named
            or ("MainThread" if _main_thread(frames) else f"thread-{ident}"),
            "daemon": False,
            "frames": [
                {**frame, "text": linecache.getline(frame["file"], frame["line"]).strip() or None}
                for frame in frames
            ],
            "request": serving.get(ident),
        }
        for ident, frames, named in threads
    ]
    found.sort(key=lambda entry: str(entry["name"]))
    return {"threads": found, "active": active, "outside": True}


class Reader:
    """Reads the workers of one host, once it knows it can."""

    def __init__(self, proc: str = PROC) -> None:
        self.unreadable: str | None = None
        self.proc = proc

    def stacks(self, pid: int) -> list[Thread]:
        """Read the worker's threads with their frames, root first.

        Each thread's name is read from the operating system as soon as
        its frames are, while the thread is most likely still there. The
        threads the runtime parks are left out.
        """

        if self.unreadable is not None:
            raise Unreadable(self.unreadable)

        unwinder = None

        # As in a profile, a read that meets a frame being pushed or
        # popped fails, so it is tried again a few times.

        for attempt in range(3):
            try:
                if unwinder is None:
                    unwinder = _unwinder(pid)

                trace = unwinder.get_stack_trace()
                break
            except Unreadable as error:
                self.unreadable = str(error)
                raise
            except PermissionError as error:
                self.unreadable = str(error)
                raise Unreadable(str(error)) from None
            except (OSError, RuntimeError) as error:
                if attempt == 2 or isinstance(error, ProcessLookupError):
                    raise Unreadable(str(error)) from None

                time.sleep(0.01)

        names = Names(pid, self.proc)
        return [
            (ident, frames, names.get(ident))
            for ident, frames in _threads(trace)
            if not parked(frames)
        ]

    def profile(
        self, pids: list[int], duration: float, interval: float, names: dict[int, dict[str, str]]
    ) -> dict[int, dict[str, Any]]:
        """Sample the workers' stacks for `duration` seconds.

        They are reported as the in-process profile reports them:
        collapsed stacks under each thread's name, a frame labelled
        `function (file:line)` by the line its function starts on.
        """

        unwinders: dict[int, Any] = {}
        failed: dict[int, str] = {}

        for pid in pids:
            try:
                unwinders[pid] = _unwinder(pid)
            except Unreadable as error:
                failed[pid] = str(error)

        counts: dict[int, Counter[str]] = {pid: Counter() for pid in unwinders}
        os_names = {pid: Names(pid, self.proc) for pid in unwinders}
        rounds: Counter[int] = Counter()
        deadline = time.monotonic() + duration

        while unwinders and time.monotonic() < deadline:
            for pid, unwinder in list(unwinders.items()):
                # A read can fail when it meets a frame the worker is
                # pushing or popping, so a failed round is skipped and the
                # worker is sampled again; the error is reported only for
                # a worker no round could read.

                try:
                    trace = unwinder.get_stack_trace()
                except (OSError, RuntimeError) as error:
                    if not rounds[pid]:
                        failed[pid] = str(error)

                    # A worker that has exited is not read again.

                    if isinstance(error, ProcessLookupError):
                        del unwinders[pid]

                    continue

                failed.pop(pid, None)

                known = names.get(pid, {})

                for ident, frames in _threads(trace):
                    # As inside, the main thread, which only waits, is
                    # not sampled.

                    if parked(frames) or ident == pid or _main_thread(frames):
                        continue

                    labels = [
                        f"{frame['function']} ({os.path.basename(frame['file'])}:"
                        f"{first_line(frame['file'], frame['line'], frame['function'])})"
                        for frame in frames
                    ]

                    # A thread the runtime does not name is named by the
                    # operating system, asked once for each thread.

                    thread = known.get(str(ident)) or os_names[pid].get(ident) or f"thread-{ident}"
                    counts[pid][";".join([thread, *labels])] += 1

                rounds[pid] += 1

            time.sleep(interval)

        results: dict[int, dict[str, Any]] = {
            pid: {"ok": False, "error": f"could not be read from outside: {error}"}
            for pid, error in failed.items()
        }

        for pid, stacks in counts.items():
            if pid not in results:
                results[pid] = {
                    "ok": True,
                    "result": {
                        "interval_ms": interval * 1000,
                        "rounds": rounds[pid],
                        "stacks": dict(stacks),
                        "outside": True,
                    },
                }

        return results
