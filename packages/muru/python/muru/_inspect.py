"""What a worker answers when its host asks (specification section 11.7).

The answers are the Python stacks of its threads with the request each
serves, the asyncio tasks of its event loop, what it has loaded and how it
is configured, and a logger's level for a period; and for phase 3, a
sampling profile or a sys.monitoring trace over a period, the memory with
an optional tracemalloc period, the output of a snippet run in the
worker, and the exchanges of a debugger session.

Each request is one JSON object with an ``op`` and ``args``; each answer
is one JSON object with ``ok`` and a ``result`` or an ``error``. The
runtime's control thread calls :func:`handle` with the request line and
the requests in flight, attached to the interpreter only for the call.
"""

from __future__ import annotations

import ast
import asyncio
import contextlib
import gc
import io
import json
import logging
import os
import sys
import threading
import time
import traceback
from collections import Counter, defaultdict
from concurrent.futures import Future
from typing import Any

from muru import _debug

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
    slots: int = 0,
) -> str:
    """Answer one request line.

    `active` lists the requests in flight, `names` the runtime's own
    threads by identifier, which Python's threading module does not know,
    and `slots` how many requests the worker serves at once.
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
        elif op == "profile":
            result = profile(
                float(args.get("duration_s", 10.0)),
                float(args.get("interval_ms", 10.0)) / 1000,
                names or {},
                int(args.get("requests", 0)),
                str(args.get("path", "")),
            )
        elif op == "trace":
            result = trace(
                float(args.get("duration_s", 10.0)),
                bool(args.get("force", False)),
                len(active),
                slots,
                int(args.get("top", 30)),
            )
        elif op == "memory":
            result = memory(
                bool(args.get("trace", False)),
                float(args.get("duration_s", 10.0)),
                int(args.get("top", 20)),
                str(args.get("snapshot", "")),
            )
        elif op == "exec":
            result = run_code(str(args.get("code", "")))
        elif op == "breakpoints":
            result = _debug.waiting()
        elif op == "attach":
            result = _debug.attach(bool(args.get("dap", False)))
        elif op == "attach_io":
            result = _debug.exchange(
                int(args.get("session", 0)),
                str(args.get("input", "")),
                float(args.get("wait_s", 0.5)),
                str(args.get("data", "")),
            )
        elif op == "detach":
            result = _debug.detach(int(args.get("session", 0)))
        else:
            raise ValueError(f"unknown operation {op!r}")
    except Exception as error:
        return json.dumps(
            {"id": request_id, "ok": False, "error": f"{type(error).__name__}: {error}"}
        )

    return json.dumps({"id": request_id, "ok": True, "result": result}, default=str)


def _parked(frame: Any) -> bool:
    """Say whether a thread waits for work in the runtime's own frame."""

    return bool(frame.f_code.co_filename.endswith(os.path.join("muru", "_pool.py")))


def _label(code: Any) -> str:
    """Name a function as a profile shows it: its qualified name and where it starts."""

    name = getattr(code, "co_qualname", code.co_name)
    return f"{name} ({os.path.basename(code.co_filename)}:{code.co_firstlineno})"


def profile(
    duration: float,
    interval: float,
    names: dict[int, str],
    requests: int = 0,
    path: str = "",
) -> dict[str, Any]:
    """Sample every thread's stack for `duration` seconds.

    Each sample of a thread running Python code counts once for its
    stack, root first and under the thread's name, in the collapsed form
    flame graph tools read. Threads waiting outside Python (idle workers
    of the pool) have no stack and are not counted, and neither is the
    main thread, which waits in the runtime for the worker's life.

    With `requests` or `path`, only the threads serving a request whose
    path starts with `path` are sampled, and with `requests` the profile
    ends once that many such requests have finished.
    """

    from muru import _native

    known = {thread.ident: thread.name for thread in threading.enumerate()}
    skipped = {threading.get_ident(), threading.main_thread().ident}
    counts: Counter[str] = Counter()
    rounds = 0
    deadline = time.monotonic() + duration
    chosen = bool(requests or path)
    seen: set[str] = set()
    finished = 0

    while time.monotonic() < deadline:
        serving: set[int] | None = None

        # Section 11.7: the requests chosen are those in flight under the
        # path; one no longer in flight has finished.

        if chosen:
            matching = {
                request["request_id"]: request["thread"]
                for request in _native.requests()
                if request["path"].startswith(path)
            }
            finished += len(seen - matching.keys())
            seen = set(matching)
            serving = {thread for thread in matching.values() if thread is not None}

            if requests and finished >= requests:
                break

        for ident, frame in sys._current_frames().items():
            if ident in skipped or (serving is not None and ident not in serving) or _parked(frame):
                continue

            stack = []
            current: Any = frame

            while current is not None:
                stack.append(_label(current.f_code))
                current = current.f_back

            thread = names.get(ident) or known.get(ident) or f"thread-{ident}"
            counts[";".join([thread, *reversed(stack)])] += 1

        rounds += 1
        time.sleep(interval)

    result: dict[str, Any] = {
        "interval_ms": interval * 1000,
        "rounds": rounds,
        "stacks": dict(counts),
    }

    if chosen:
        result["requests"] = finished

    return result


def trace(duration: float, force: bool, busy: int, slots: int, top: int) -> dict[str, Any]:
    """Count calls and time every Python function with sys.monitoring.

    Exact, unlike sampling, and costly: a worker serving requests in more
    than half its slots is refused unless `force` is set.
    """

    monitoring = getattr(sys, "monitoring", None)

    if monitoring is None:
        raise RuntimeError("trace needs Python 3.12 or later, for sys.monitoring")

    if not force and slots and busy * 2 > slots:
        raise RuntimeError(
            f"the worker is busy ({busy} of {slots} slots); --force traces it anyway"
        )

    tool = monitoring.PROFILER_ID
    monitoring.use_tool_id(tool, "muru")
    calls: Counter[Any] = Counter()
    spent: defaultdict[Any, int] = defaultdict(int)
    local = threading.local()

    def enter(code: Any, offset: int) -> None:
        stack = getattr(local, "stack", None)

        if stack is None:
            stack = local.stack = []

        stack.append((code, time.perf_counter_ns()))

    def started(code: Any, offset: int) -> None:
        calls[code] += 1
        enter(code, offset)

    def leave(code: Any, offset: int, value: Any) -> None:
        stack = getattr(local, "stack", None)

        if stack:
            entered, since = stack.pop()
            spent[entered] += time.perf_counter_ns() - since

    events = monitoring.events
    callbacks = [
        (events.PY_START, started),
        (events.PY_RESUME, enter),
        (events.PY_RETURN, leave),
        (events.PY_YIELD, leave),
        (events.PY_UNWIND, leave),
    ]

    for event, callback in callbacks:
        monitoring.register_callback(tool, event, callback)

    try:
        monitoring.set_events(tool, sum(event for event, _ in callbacks))
        time.sleep(duration)
    finally:
        monitoring.set_events(tool, 0)

        for event, _ in callbacks:
            monitoring.register_callback(tool, event, None)

        monitoring.free_tool_id(tool)

    ranked = sorted(spent.items(), key=lambda item: item[1], reverse=True)[:top]
    return {
        "functions": [
            {
                "function": _label(code),
                "calls": calls[code],
                "total_ms": round(total / 1_000_000, 3),
            }
            for code, total in ranked
        ]
    }


# Section 11.7: a tracemalloc snapshot kept from one request to a later
# one, so the allocations between two chosen points can be compared, and
# whether keeping it started tracemalloc.

_kept: Any = None
_kept_started = False


def _sites(after: Any, before: Any, top: int) -> list[dict[str, Any]]:
    """List the sites that allocated most between two snapshots."""

    return [
        {
            "site": str(stat.traceback[0]) if stat.traceback else "?",
            "size_diff": stat.size_diff,
            "count_diff": stat.count_diff,
            "size": stat.size,
        }
        for stat in after.compare_to(before, "lineno")[:top]
    ]


def snapshot(action: str, top: int) -> dict[str, Any]:
    """Keep a tracemalloc snapshot, compare the present with it, or drop it.

    `take` starts tracemalloc when it is off and keeps a snapshot, in
    place of one kept before; `diff` reports the sites that allocated
    most since it was taken, keeping it; `drop` lets it go and stops
    tracemalloc when taking it started it.
    """

    import tracemalloc

    global _kept, _kept_started

    if action == "take":
        if not tracemalloc.is_tracing():
            tracemalloc.start(10)
            _kept_started = True

        _kept = tracemalloc.take_snapshot()
        current, peak = tracemalloc.get_traced_memory()
        return {"snapshot": "taken", "traced": current, "peak": peak}

    if action == "diff":
        if _kept is None or not tracemalloc.is_tracing():
            raise RuntimeError("no snapshot is kept; take one with --snapshot")

        return {
            "snapshot": "compared",
            "allocations": _sites(tracemalloc.take_snapshot(), _kept, top),
        }

    if action == "drop":
        had = _kept is not None
        _kept = None

        if _kept_started:
            tracemalloc.stop()
            _kept_started = False

        return {"snapshot": "dropped" if had else "none"}

    raise ValueError(f"unknown snapshot action {action!r}")


def memory(tracing: bool, duration: float, top: int, keep: str = "") -> dict[str, Any]:
    """Report the worker's memory, the gc's state and the most common objects.

    With `tracing`, tracemalloc runs for `duration` seconds and the sites
    that allocated most in that time are reported too. With `keep`, a
    snapshot is taken, compared with or dropped (see `snapshot`) instead.
    """

    from muru import _native

    result: dict[str, Any] = {
        "memory": _native.memory_usage(),
        "gc": {"counts": list(gc.get_count()), "collections": gc.get_stats()},
        "objects": Counter(type(item).__qualname__ for item in gc.get_objects()).most_common(top),
    }

    if keep:
        result.update(snapshot(keep, top))
        return result

    if tracing:
        import tracemalloc

        started = not tracemalloc.is_tracing()

        if started:
            tracemalloc.start(10)

        try:
            before = tracemalloc.take_snapshot()
            time.sleep(duration)
            after = tracemalloc.take_snapshot()
        finally:
            if started:
                tracemalloc.stop()

        result["allocations"] = _sites(after, before, top)

    return result


def run_code(code: str) -> dict[str, Any]:
    """Run a snippet in the worker and return what it printed and its value.

    The last statement's value, when it is an expression, is returned as
    its repr; what the snippet prints, or raises, comes back as output.
    """

    tree = ast.parse(code, "<muructl debug exec>", "exec")
    final = tree.body[-1] if tree.body else None
    last = final if isinstance(final, ast.Expr) else None

    if last is not None:
        tree.body.pop()
    namespace: dict[str, Any] = {"__name__": "__muru_exec__"}
    output = io.StringIO()
    value: Any = None

    with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
        try:
            exec(compile(tree, "<muructl debug exec>", "exec"), namespace)

            if last is not None:
                expression = ast.Expression(last.value)
                value = eval(compile(expression, "<muructl debug exec>", "eval"), namespace)
        except BaseException:
            traceback.print_exc()

    return {"output": output.getvalue(), "value": None if value is None else repr(value)}


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
        if ident == own or _parked(frame):
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
