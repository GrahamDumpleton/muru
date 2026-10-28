"""Reading a worker from outside (specification section 11.7).

PEP 768 lets another process read a running interpreter's frames on
Python 3.14 and later, without the interpreter's cooperation, so the host
can see the stacks of a worker whose threads hold the interpreter or
spin. The host is the reader: it is the workers' parent and runs as their
user, which Linux's default ptrace scope allows. macOS allows it only to
root, so there the host falls back to asking the worker itself, as it
does on older interpreters.

Threads read from outside have identifiers but no names; the worker's
bookkeeping, which the runtime answers without the interpreter, supplies
the names of its own threads and the request each thread serves.
"""

from __future__ import annotations

import linecache
import os
import time
from collections import Counter
from typing import Any


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


def parked(frames: list[dict[str, Any]]) -> bool:
    """Say whether a thread is one the runtime parks.

    Such a thread has no Python frames, or waits for work in the
    runtime's own frame (`muru._pool`).
    """

    return not frames or frames[-1]["file"].endswith(os.path.join("muru", "_pool.py"))


def _main_thread(frames: list[dict[str, Any]]) -> bool:
    """Say whether a thread is the worker's main thread.

    It waits in the runtime, called from the host's module, for the
    worker's life.
    """

    return bool(frames) and frames[-1]["file"].endswith(os.path.join("muru", "host.py"))


def stacks_result(
    threads: list[tuple[int, list[dict[str, Any]]]], bookkeeping: dict[str, Any]
) -> dict[str, Any]:
    """Report threads read from outside as the in-process stacks do.

    The bookkeeping names them and gives the request each serves.
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
    found = [
        {
            "ident": ident,
            "name": names.get(
                str(ident), "MainThread" if _main_thread(frames) else f"thread-{ident}"
            ),
            "daemon": False,
            "frames": [
                {**frame, "text": linecache.getline(frame["file"], frame["line"]).strip() or None}
                for frame in frames
            ],
            "request": serving.get(ident),
        }
        for ident, frames in threads
    ]
    found.sort(key=lambda entry: str(entry["name"]))
    return {"threads": found, "active": active, "outside": True}


class Reader:
    """Reads the workers of one host, once it knows it can."""

    def __init__(self) -> None:
        self.unreadable: str | None = None

    def stacks(self, pid: int) -> list[tuple[int, list[dict[str, Any]]]]:
        """Read the worker's threads with their frames, root first.

        The threads the runtime parks are left out.
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

        return [(ident, frames) for ident, frames in _threads(trace) if not parked(frames)]

    def profile(
        self, pids: list[int], duration: float, interval: float, names: dict[int, dict[str, str]]
    ) -> dict[int, dict[str, Any]]:
        """Sample the workers' stacks for `duration` seconds.

        They are reported as the in-process profile reports them:
        collapsed stacks under each thread's name, a frame labelled
        `function (file:line)` by the line it was running.
        """

        unwinders: dict[int, Any] = {}
        failed: dict[int, str] = {}

        for pid in pids:
            try:
                unwinders[pid] = _unwinder(pid)
            except Unreadable as error:
                failed[pid] = str(error)

        counts: dict[int, Counter[str]] = {pid: Counter() for pid in unwinders}
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

                    if parked(frames) or _main_thread(frames):
                        continue

                    labels = [
                        f"{frame['function']} ({os.path.basename(frame['file'])}:{frame['line']})"
                        for frame in frames
                    ]
                    thread = known.get(str(ident), f"thread-{ident}")
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
