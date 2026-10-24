"""What a worker answers when its host asks (specification section 11.7).

The answers are the Python stacks of its threads with the request each
serves, the asyncio tasks of its event loop, what it has loaded and how it
is configured, and a logger's level for a period.

Each request is one JSON object with an ``op`` and ``args``; each answer
is one JSON object with ``ok`` and a ``result`` or an ``error``. The
runtime's control thread calls :func:`handle` with the request line and
the requests in flight, attached to the interpreter only for the call.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import sys
import threading
import time
import traceback
from concurrent.futures import Future
from typing import Any

# Environment keys whose values are not shown.

REDACTED = ("SECRET", "TOKEN", "PASSWORD", "PASSWD", "KEY", "CREDENTIAL", "AUTH")

# Logger levels changed for a period, with what they were, so a reset or
# the period's end puts them back.

_changed: dict[str, int] = {}
_timers: dict[str, threading.Timer] = {}
_lock = threading.Lock()


def handle(
    line: str,
    active: list[dict[str, Any]],
    loop: asyncio.AbstractEventLoop | None = None,
    names: dict[int, str] | None = None,
) -> str:
    """Answer one request line.

    `active` lists the requests in flight and `names` the runtime's own
    threads by identifier, which Python's threading module does not know.
    """

    request_id: Any = None

    try:
        request = json.loads(line)
        request_id = request.get("id")
        op = request.get("op", "")
        args = request.get("args") or {}

        if op == "stacks":
            result = stacks(active, names or {})
        elif op == "tasks":
            result = tasks(loop)
        elif op == "inspect":
            result = inspect()
        elif op == "logging":
            result = set_level(
                str(args.get("logger", "")),
                str(args.get("level", "")),
                args.get("duration_s"),
                bool(args.get("reset", False)),
            )
        else:
            raise ValueError(f"unknown operation {op!r}")
    except Exception as error:
        return json.dumps(
            {"id": request_id, "ok": False, "error": f"{type(error).__name__}: {error}"}
        )

    return json.dumps({"id": request_id, "ok": True, "result": result}, default=str)


def stacks(active: list[dict[str, Any]], names: dict[int, str]) -> dict[str, Any]:
    """Collect every thread's Python stack, with the request the thread serves.

    The thread answering this request is left out, since its stack is
    only this call.
    """

    known = {thread.ident: thread for thread in threading.enumerate()}
    serving = {entry["thread"]: entry for entry in active if entry.get("thread") is not None}
    threads = []
    own = threading.get_ident()

    for ident, frame in sys._current_frames().items():
        if ident == own:
            continue

        thread = known.get(ident)
        frames = [
            {"file": item.filename, "line": item.lineno, "function": item.name, "text": item.line}
            for item in traceback.extract_stack(frame)
        ]
        name = thread.name if thread is not None else names.get(ident, f"thread-{ident}")
        threads.append(
            {
                "ident": ident,
                "name": name,
                "daemon": thread.daemon if thread is not None else False,
                "frames": frames,
                "request": serving.get(ident),
            }
        )

    threads.sort(key=lambda entry: str(entry["name"]))
    return {"threads": threads, "active": active}


def tasks(loop: asyncio.AbstractEventLoop | None) -> dict[str, Any]:
    """Collect the event loop's tasks, what each awaits, and the loop's lag."""

    if loop is None:
        return {"tasks": [], "lag_ms": None, "note": "this worker has no event loop"}

    collected: Future[list[dict[str, Any]]] = Future()
    scheduled = time.monotonic()

    def collect() -> None:
        lag_ms = (time.monotonic() - scheduled) * 1000
        entries = []

        for task in asyncio.all_tasks(loop):
            coro = task.get_coro()
            waiting = getattr(task, "_fut_waiter", None)
            frames = task.get_stack(limit=1)
            where = (
                {"file": frames[0].f_code.co_filename, "line": frames[0].f_lineno}
                if frames
                else None
            )
            entries.append(
                {
                    "name": task.get_name(),
                    "coro": getattr(coro, "__qualname__", repr(coro)),
                    "done": task.done(),
                    "waiting_on": repr(waiting) if waiting is not None else None,
                    "at": where,
                }
            )

        entries.sort(key=lambda entry: str(entry["name"]))
        collected.set_result([{"lag_ms": round(lag_ms, 1)}, *entries])

    loop.call_soon_threadsafe(collect)
    entries = collected.result(timeout=2.0)
    return {"lag_ms": entries[0]["lag_ms"], "tasks": entries[1:]}


def inspect() -> dict[str, Any]:
    """Describe the worker's interpreter, modules, environment, threads and descriptors."""

    from muru import context

    modules = {}

    for name, module in sorted(sys.modules.items()):
        if module is None or "." in name:
            continue

        modules[name] = {
            "version": getattr(module, "__version__", None),
            "file": getattr(module, "__file__", None),
        }

    environment = {
        key: ("***" if any(word in key.upper() for word in REDACTED) else value)
        for key, value in sorted(os.environ.items())
    }
    threads = [
        {"name": thread.name, "ident": thread.ident, "daemon": thread.daemon}
        for thread in threading.enumerate()
    ]
    current = context()

    return {
        "pid": os.getpid(),
        "python": sys.version.split()[0],
        "executable": sys.executable,
        "cwd": os.getcwd(),
        "sys_path": list(sys.path),
        "modules": modules,
        "environment": environment,
        "threads": threads,
        "descriptors": descriptors(),
        "settings": {
            "app": current.app,
            "group": current.group,
            "worker": current.worker_id,
            "process_model": current.process_model,
            "gil_enabled": getattr(sys, "_is_gil_enabled", lambda: True)(),
        },
    }


def descriptors() -> dict[str, Any]:
    """Open descriptors: how many, and what each is where the platform tells."""

    for directory in ("/proc/self/fd", "/dev/fd"):
        try:
            names = os.listdir(directory)
        except OSError:
            continue

        entries = []

        for name in names:
            try:
                target = os.readlink(os.path.join(directory, name))
            except OSError:
                target = None

            entries.append({"fd": int(name), "target": target})

        entries.sort(key=lambda entry: int(entry["fd"] or 0))
        return {"count": len(entries), "open": entries}

    return {"count": None, "open": []}


def set_level(logger: str, level: str, duration_s: float | None, reset: bool) -> dict[str, Any]:
    """Set a logger's level, reverting after `duration_s` or on reset."""

    with _lock:
        if reset:
            restored = []

            for name, previous in list(_changed.items()):
                logging.getLogger(name or None).setLevel(previous)
                restored.append(name or "root")
                timer = _timers.pop(name, None)

                if timer is not None:
                    timer.cancel()

            _changed.clear()
            return {"reset": restored}

        numeric = logging.getLevelName(level.upper())

        if not isinstance(numeric, int):
            raise ValueError(f"{level!r} is not a logging level")

        target = logging.getLogger(logger or None)
        key = logger
        previous = _changed.get(key, target.level)
        _changed.setdefault(key, target.level)
        target.setLevel(numeric)
        timer = _timers.pop(key, None)

        if timer is not None:
            timer.cancel()

        if duration_s is not None:
            timer = threading.Timer(float(duration_s), _revert, args=(key,))
            timer.daemon = True
            timer.start()
            _timers[key] = timer

    return {
        "logger": logger or "root",
        "level": logging.getLevelName(numeric),
        "previous": logging.getLevelName(previous),
        "for_s": duration_s,
    }


def _revert(key: str) -> None:
    with _lock:
        previous = _changed.pop(key, None)
        _timers.pop(key, None)

        if previous is not None:
            logging.getLogger(key or None).setLevel(previous)
