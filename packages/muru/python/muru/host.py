"""Host process entry point, run as ``python -m muru.host``.

The host preloads the application, forks and supervises workers (design
specification sections 6.1 to 6.3). It is single-threaded: no threads, event
loop or Rust runtime exist before fork. Each worker enables garbage collection
after fork and serves the worker link on its own Unix socket through the
native extension.

Preload has three modes. With auto, the default, the host imports the
application and runs the import check of section 6.2; if the import left
behind something fork cannot share, the host logs what it found and
re-executes itself with per-worker imports. With true such a finding is a
failure, and with false every worker imports for itself and the check is
skipped. Hooks come from the module named with --hooks.

Each worker writes to pipes the host created at fork, and the host relays
every line to its own standard error tagged with the worker's index and
stream (section 11.2), so output from different workers can be told apart.

One application, a fixed number of workers restarted when they die, and
shutdown on SIGTERM, SIGINT or when standard input closes, which is how the
parent `muructl` signals that it has gone away. Under the daemon the host
writes to the file named with --log-file: SIGUSR1 opens it again by name
after an external rotation (section 11.3), and SIGUSR2 passes SIGUSR1 on
to every worker, whose on_reopen_logs hook handles the application's own
files.

Each worker reports to the host once a second over a pipe: requests in
flight, the age of the oldest, requests served, and at intervals its
memory. From those the host enforces the hard timeout of section 7.6 and
the recycling and memory limits of section 7.7: a worker past its limit
is replaced by a fresh one on a new socket,
and the supervisor (the daemon, or `muructl run`) learns the new socket
over the host's control socket, named with --control, so the edge routes
to it before the old worker is told to stop.
"""

from __future__ import annotations

import argparse
import faulthandler
import gc
import importlib
import inspect
import json
import os
import random
import selectors
import signal
import socket
import sys
import threading
import time
import traceback
from collections.abc import Callable
from dataclasses import dataclass, field, replace
from types import ModuleType
from typing import Any

from muru import RequestTimeout, _asgi, _inspect, _outside, preload
from muru import logging as muru_logging
from muru._context import Context
from muru._context import _set as set_context
from muru.hooks import Hooks

WSGIApplication = Callable[..., Any]

RESTART_DELAY_SECONDS = 0.5

# How long a replacement waits for the supervisor to say the edge routes
# to the new worker before the old one is told to stop regardless, and
# how long after that word the old worker is left alone, so a request
# the edge had already dispatched to it by the old routing is served.

ACK_TIMEOUT_SECONDS = 5.0
ROUTE_SETTLE_SECONDS = 1.0

# How long a stalled worker already being replaced gets to give up its
# requests before it is killed.

STALLED_KILL_SECONDS = 1.0

# How long after a forced restart takes a worker out of routing it is
# killed: time enough for its stacks to be taken.

FORCE_KILL_SECONDS = 1.0

# How often the supervision loop runs when timeouts or recycling are on.

TICK_SECONDS = 0.5

# How often a worker's native side writes a report, as the link has it:
# a worker is silent once a report is this much later than due.

REPORT_INTERVAL_SECONDS = 1.0

# How long a worker is given to answer a control request before the
# host answers for it.

CONTROL_TIMEOUT_SECONDS = 5.0

# Memory recycles of one worker index within this window raise a leak
# warning, once per window.

LEAK_WINDOW_SECONDS = 3600.0
LEAK_RECYCLES = 3

# A worker exiting with an error this soon after its fork failed to start.

STARTUP_WINDOW_SECONDS = 5.0

# Consecutive start failures of one worker before the host gives up.

MAX_START_FAILURES = 3
PRELOAD_MODES = ("auto", "true", "false")
READ_SIZE = 65536


# Set in a worker, whose lines the host relays with the worker's own tag,
# so the host prefix is for the host alone.

_in_worker = False


def log(message: str) -> None:
    """Write one line to standard error, prefixed so it can be told from application output."""

    prefix = "" if _in_worker else f"muru.host[{os.getpid()}]: "

    # The parent that reads this stream may already be gone, and a log line
    # lost then must not stop the host from stopping its workers.

    try:
        print(f"{prefix}{message}", file=sys.stderr, flush=True)
    except OSError:
        pass


INTERFACES = ("auto", "wsgi", "asgi", "grpc")

# The longest a Unix socket path may be, its NUL included: sun_path is 104
# bytes on macOS and 108 on Linux.

SOCKET_PATH_LIMIT = 104 if sys.platform == "darwin" else 108
FACTORY_PHASES = ("worker", "host")
LOG_LEVELS = ("DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL")


def resolve_app(
    spec: str, import_module: Callable[[str], ModuleType] = importlib.import_module
) -> Any:
    """Import ``module:attribute`` and return the callable.

    The attribute defaults to ``application``. The result is the application
    itself, or a factory when the release says so.
    """

    module_name, _, attribute = spec.partition(":")
    if not module_name:
        raise ValueError(f"invalid application {spec!r}: expected module:callable")
    attribute = attribute or "application"
    target: Any = import_module(module_name)
    for part in attribute.split("."):
        target = getattr(target, part)
    if not callable(target):
        raise TypeError(f"{spec!r} is not callable")
    return target


def detect_interface(app: Any) -> str:
    """Return ``asgi`` for a coroutine function or an awaitable ``__call__``, else ``wsgi``."""

    if inspect.iscoroutinefunction(app):
        return "asgi"
    call = getattr(app, "__call__", None)  # noqa: B004  (a bound method, not the type's slot)
    if call is not None and inspect.iscoroutinefunction(call):
        return "asgi"
    return "wsgi"


def interface_name(interface: str) -> str:
    """Return an interface as messages name it: WSGI, ASGI or gRPC."""

    return "gRPC" if interface == "grpc" else interface.upper()


# What a call passes: environ and start_response, or scope, receive and send.

ARGUMENTS = {"wsgi": 2, "asgi": 3}


def positional_shape(app: Any) -> tuple[int, int, str | None] | None:
    """Return the positional arguments ``app`` needs and takes, and a required keyword-only name.

    The signature is the callable's own: a class is read through its
    constructor, since calling it is what serves a request, and an object
    through its ``__call__``. ``*args`` takes any number. None when Python
    cannot tell, as for some callables written in C, which are then taken
    as they are.
    """

    try:
        signature = inspect.signature(app)
    except (TypeError, ValueError):
        return None

    required = accepted = 0
    keyword = None

    for parameter in signature.parameters.values():
        if parameter.kind in (parameter.POSITIONAL_ONLY, parameter.POSITIONAL_OR_KEYWORD):
            accepted += 1

            if parameter.default is parameter.empty:
                required += 1
        elif parameter.kind is parameter.VAR_POSITIONAL:
            accepted = sys.maxsize
        elif parameter.kind is parameter.KEYWORD_ONLY and parameter.default is parameter.empty:
            keyword = keyword or parameter.name

    return required, accepted, keyword


def check_arguments(spec: str, app: Any, interface: str, declared: bool) -> None:
    """Refuse ``app`` when it cannot be called as a ``interface`` application is.

    A WSGI application is called with two positional arguments and an
    ASGI application with three. A callable with no required positional
    parameters is most likely an application factory named without the
    factory setting, which would otherwise fail on every request; one
    that fits the other interface is refused rather than guessed at.
    """

    shape = positional_shape(app)

    if shape is None:
        return

    required, accepted, keyword = shape
    wanted = ARGUMENTS[interface]
    name = interface_name(interface)

    if keyword is None and required <= wanted <= accepted:
        return

    if keyword is not None:
        raise RuntimeError(
            f"{spec} has a required keyword-only parameter {keyword!r}, which a {name} "
            "application is never passed"
        )

    if required == 0 and accepted < wanted:
        raise RuntimeError(
            f"{spec} takes no required arguments, so it cannot be a {name} application; if it "
            "is an application factory, set factory = true in [tool.muru], or give --factory "
            "when the entrypoint is named on the command line"
        )

    other = "asgi" if interface == "wsgi" else "wsgi"
    other_name = interface_name(other)
    takes = f"takes {ARGUMENTS[other]} arguments, as an {other_name} application does"

    if required <= ARGUMENTS[other] <= accepted:
        if declared:
            raise RuntimeError(f"{spec} is declared {name} but {takes}; declare it as {other}")

        if interface == "asgi":
            raise RuntimeError(
                f"{spec} is a coroutine function, so it cannot be served as WSGI, but {takes}"
            )

        raise RuntimeError(
            f"{spec} {takes}, but is not a coroutine function, so it was detected as WSGI; "
            "declare interface = 'asgi' if it is one"
        )

    if accepted == sys.maxsize:
        count = f"at least {required}"
    elif required == accepted:
        count = str(required)
    else:
        count = f"{required} to {accepted}"

    raise RuntimeError(
        f"{spec} takes {count} positional arguments, where a {name} application is passed {wanted}"
    )


def resolve_interface(spec: str, app: Any, interface: str) -> str:
    """Return the interface the application is served with: ``wsgi``, ``asgi`` or ``grpc``.

    ``auto`` detects WSGI or ASGI from the callable; ``grpc`` is never
    detected, since its entrypoint is a registration function (section
    6.13). A declared interface is trusted, except that an application
    declared WSGI whose callable is a coroutine function cannot be served
    that way and is refused. Either way the callable must take the
    arguments the interface passes, which catches a factory named without
    the factory setting at load rather than on every request.
    """

    if interface == "grpc":
        return interface

    detected = detect_interface(app)

    if interface == "wsgi" and detected == "asgi":
        raise RuntimeError(
            f"{spec} is declared WSGI but is a coroutine function; declare it as asgi or "
            "leave the interface to be detected"
        )

    resolved = detected if interface == "auto" else interface
    check_arguments(spec, app, resolved, declared=interface != "auto")
    return resolved


def app_name(spec: str) -> str:
    """Name the application after the top-level module of its entrypoint."""

    module_name = spec.partition(":")[0]
    return module_name.split(".")[0] or "default"


def process_model(threads: int, interface: str = "wsgi") -> str:
    """Name the process model of section 6.4 for a release."""

    if interface in ("asgi", "grpc"):
        return "async"

    gil_enabled = getattr(sys, "_is_gil_enabled", lambda: True)()
    if not gil_enabled:
        return "free-threaded"
    return "process-per-request" if threads == 1 else "threaded"


@dataclass
class Release:
    """What the host runs: the application and its hooks, loaded or still to load."""

    spec: str
    hooks_module: str | None
    context: Context
    threads: int = 1
    interface: str = "auto"
    factory: bool = False
    factory_phase: str = "worker"
    target: Any = None
    app: Any = None
    hooks: Hooks | None = None
    resolved: str | None = None
    """The interface the application is served with, once it is known."""

    @property
    def preloaded(self) -> bool:
        """Whether the entrypoint was imported in the host."""

        return self.target is not None

    def load(self, recorder: preload.ImportRecorder | None = None) -> Hooks:
        """Import the hooks module and the entrypoint, in that order.

        A factory with factory_phase ``host`` is called here (section 6.8);
        one with phase ``worker`` waits for `realise` in each worker.
        """

        import_module = recorder.import_module if recorder else importlib.import_module
        if self.hooks_module:
            import_module(self.hooks_module)
        self.hooks = Hooks.load(self.hooks_module)
        self.target = resolve_app(self.spec, import_module)

        if not self.factory:
            self.accept(self.target)
        elif self.factory_phase == "host":
            self.accept(self.target())

        return self.hooks

    def accept(self, app: Any) -> None:
        """Keep the application and settle the interface it is served with.

        A gRPC entrypoint is called now with the server it registers its
        services on (section 6.13), which is what is served.
        """

        self.resolved = resolve_interface(self.spec, app, self.interface)

        if self.resolved == "grpc":
            from muru import _grpc

            app = _grpc.register(app)

        self.app = app

        # The context tells hooks and the application what they run under.

        self.context = replace(
            self.context,
            interface=self.resolved,
            process_model=process_model(self.threads, self.resolved),
        )

    def realise(self) -> Any:
        """Return the application to serve, calling a per-worker factory now."""

        if self.app is None:
            if self.target is None:
                raise RuntimeError(f"{self.spec} was not loaded")
            self.accept(self.target())

        return self.app


@dataclass
class WorkerOutput:
    """The read end of one worker's stdout or stderr pipe, relayed line by line."""

    index: int
    stream: str
    fd: int
    pending: bytes = b""

    @property
    def tag(self) -> str:
        """The prefix each relayed line carries."""

        suffix = "" if self.stream == "stderr" else f".{self.stream}"
        return f"muru.worker[{self.index}]{suffix}: "

    def relay(self, data: bytes) -> None:
        """Write the complete lines in data, keeping any partial line for later."""

        self.pending += data
        *lines, self.pending = self.pending.split(b"\n")
        for line in lines:
            sys.stderr.write(self.tag + line.decode("utf-8", "replace") + "\n")
        sys.stderr.flush()

    def flush(self) -> None:
        """Write a final partial line, if any."""

        if self.pending:
            sys.stderr.write(self.tag + self.pending.decode("utf-8", "replace") + "\n")
            sys.stderr.flush()
            self.pending = b""


@dataclass
class Mount:
    """A named mount (section 6.7): a path served by another entrypoint.

    The mount's application is loaded and realised like the release's own,
    without hooks of its own, and shares the worker's loop. It shares the
    worker's threads too, unless it has threads of its own: the threads
    it reserves and the most requests it runs at once. An ASGI mount may
    have limits of its own instead: the requests it runs at once and the
    open WebSockets it holds, 0 for one it does not set, which the edge
    holds the slots for.
    """

    name: str
    path: str
    release: Release
    threads: tuple[int, int] | None = None
    limits: tuple[int, int] | None = None

    @classmethod
    def parse(cls, text: str, context: Context, threads: int) -> Mount:
        """Parse NAME=PATH=ENTRYPOINT[=INTERFACE[=RESERVED,MAX]].

        A gRPC mount's PATH is empty, and for an ASGI mount the last part
        is CONCURRENCY,CONNECTIONS instead.
        """

        form = "NAME=PATH=ENTRYPOINT[=INTERFACE[=RESERVED,MAX]]"
        parts = text.split("=")
        interface = parts[3] if len(parts) >= 4 else "auto"
        path_given = parts[1].startswith("/") if len(parts) > 1 else False

        if len(parts) not in (3, 4, 5) or not parts[0] or path_given == (interface == "grpc"):
            raise argparse.ArgumentTypeError(f"{text!r} is not {form}")

        if interface not in INTERFACES:
            raise argparse.ArgumentTypeError(f"{interface!r} is not an interface")

        own: tuple[int, int] | None = None
        limits: tuple[int, int] | None = None

        if len(parts) == 5:
            try:
                first, second = (int(part) for part in parts[4].split(","))
            except ValueError:
                raise argparse.ArgumentTypeError(f"{text!r} is not {form}") from None

            if interface == "asgi":
                if first < 0 or second < 0:
                    raise argparse.ArgumentTypeError(f"{text!r} has limits below nothing")

                limits = (first, second)
            else:
                if first < 0 or second < max(first, 1):
                    raise argparse.ArgumentTypeError(f"{text!r} has threads that cannot run")

                own = (first, second)

        release = Release(parts[2], None, context, threads=threads, interface=interface)
        return cls(parts[0], parts[1], release, own, limits)


@dataclass(frozen=True)
class Group:
    """A worker group (section 7.1): its name, sizes and policies.

    The default group, named "default", carries the release's own
    settings; named groups follow it in the order given. A timeout or
    memory policy left None is the release's, filled in by the host.
    """

    name: str
    processes: int
    threads: int
    sync_threads: int
    concurrency: int
    soft_timeout: float | None = None
    hard_timeout: float | None = None
    hard_drain_grace: float | None = None
    loop_stall_timeout: float | None = None
    gil_stall_timeout: float | None = None
    memory: MemoryPolicy | None = None

    # ADR 0033: the release is ready without an optional group's workers,
    # which enter routing as each listens, and a worker of one that
    # cannot start is given up rather than stopping the host.

    optional: bool = False

    # ADR 0033: the named mounts the group's workers serve beside the
    # default mount; every mount when empty.

    mounts: tuple[str, ...] = ()

    @classmethod
    def parse(cls, text: str) -> Group:
        """Parse NAME=PROCESSES,THREADS,SYNC_THREADS,CONCURRENCY."""

        name, _, sizes = text.partition("=")

        try:
            processes, threads, sync_threads, concurrency = (int(part) for part in sizes.split(","))
        except ValueError:
            raise argparse.ArgumentTypeError(
                f"{text!r} is not NAME=PROCESSES,THREADS,SYNC_THREADS,CONCURRENCY"
            ) from None

        if not name or name == "default" or processes < 1:
            raise argparse.ArgumentTypeError(f"{text!r} is not a valid worker group")

        return cls(name, processes, threads, sync_threads, concurrency)


def parse_group_mounts(text: str) -> tuple[str, tuple[str, ...]]:
    """Parse NAME=MOUNT,... into the group's name and the mounts it serves."""

    name, _, names = text.partition("=")
    mounts = tuple(part for part in names.split(",") if part)

    if not name or not mounts:
        raise argparse.ArgumentTypeError(f"{text!r} is not NAME=MOUNT,...")

    return name, mounts


def parse_group_timeouts(text: str) -> tuple[str, dict[str, float | None]]:
    """Parse NAME=SOFT,HARD,HARD_DRAIN_GRACE,LOOP_STALL,GIL_STALL, seconds or off."""

    form = "NAME=SOFT,HARD,HARD_DRAIN_GRACE,LOOP_STALL,GIL_STALL"
    name, _, values = text.partition("=")
    names = (
        "soft_timeout",
        "hard_timeout",
        "hard_drain_grace",
        "loop_stall_timeout",
        "gil_stall_timeout",
    )
    parts = values.split(",")

    if not name or len(parts) != len(names):
        raise argparse.ArgumentTypeError(f"{text!r} is not {form}")

    try:
        parsed = {
            key: None if part == "off" else float(part)
            for key, part in zip(names, parts, strict=True)
        }
    except ValueError:
        raise argparse.ArgumentTypeError(f"{text!r} is not {form}") from None

    return name, parsed


def parse_group_memory(text: str) -> tuple[str, MemoryPolicy]:
    """Parse NAME=MEASURE,SOFT_LIMIT,SOFT_FOR,RECLAIM_FIRST,HARD_LIMIT,CHECK_INTERVAL."""

    form = "NAME=MEASURE,SOFT_LIMIT,SOFT_FOR,RECLAIM_FIRST,HARD_LIMIT,CHECK_INTERVAL"
    name, _, values = text.partition("=")
    parts = values.split(",")

    if not name or len(parts) != 6:
        raise argparse.ArgumentTypeError(f"{text!r} is not {form}")

    try:
        policy = MemoryPolicy(
            measure=parts[0],
            soft_limit=None if parts[1] == "off" else int(parts[1]),
            soft_for=float(parts[2]),
            reclaim_first=parts[3] == "true",
            hard_limit=None if parts[4] == "off" else int(parts[4]),
            check_interval=float(parts[5]),
        )
    except ValueError:
        raise argparse.ArgumentTypeError(f"{text!r} is not {form}") from None

    return name, policy


@dataclass
class MemoryPolicy:
    """The memory limits of section 7.7, in bytes and seconds."""

    measure: str = "uss"
    soft_limit: int | None = None
    soft_for: float = 120.0
    reclaim_first: bool = True
    hard_limit: int | None = None
    check_interval: float = 5.0

    @property
    def active(self) -> bool:
        """Whether any limit is set."""

        return self.soft_limit is not None or self.hard_limit is not None


@dataclass
class WorkerState:
    """What the host knows about one worker from its reports."""

    index: int
    pid: int
    generation: int
    path: str
    started_at: float
    report_fd: int
    requests: int = 0
    in_flight: int = 0
    oldest_ms: int = 0
    ready: bool = False

    # Section 7.6: when the worker last reported, so one that has gone
    # silent altogether is noticed.

    last_report_at: float = 0.0

    # The request count and the time at which this worker is recycled,
    # jittered so workers do not all go at once, and whether it is the
    # old one of a replacement.

    recycle_after_requests: int | None = None
    recycle_at: float | None = None
    replaced: bool = False

    # Section 7.7: the last memory figures, when the worker went over the
    # soft limit, and when it was asked to reclaim.

    memory: dict[str, int] = field(default_factory=dict)
    over_soft_since: float | None = None
    reclaimed_at: float | None = None

    # Section 7.3: the last scaling signals, relayed to the supervisor as
    # the worker reported them.

    signals: dict[str, Any] = field(default_factory=dict)

    # Section 11.7: the debugger sessions open in the worker, which keep
    # it out of routing and from being replaced.

    debug_sessions: int = 0


@dataclass
class Pending:
    """A supervisor's request to the workers, waiting for their answers."""

    id: int
    op: str
    expected: dict[int, int]  # pid to index
    answers: dict[int, dict[str, Any]] = field(default_factory=dict)
    deadline: float = 0.0

    # Section 11.7: a request the host makes for itself, whose answers go
    # here rather than to the supervisor.

    then: Callable[[Pending], None] | None = None


@dataclass
class Replacement:
    """A worker being replaced by a fresh one (sections 7.6 and 7.7)."""

    index: int
    old_pid: int
    reason: str
    kind: str  # worker.recycled or worker.timeout_killed
    cause: str = ""  # the reason as one word, for muru_worker_recycles_total
    new_pid: int | None = None

    # Section 11.7: what the worker was doing when it was replaced (its
    # stuck request's stack, or its commonest objects), for the event.

    capture: str | None = None
    announced: int | None = None
    announced_at: float | None = None

    # A stall found after the condemnation ends the grace (section 7.6).

    stalled: bool = False
    terminate_at: float | None = None
    terminated: bool = False
    kill_at: float | None = None
    killed: bool = False

    # Section 7.4: a retirement, which stops the worker without a successor.

    retire: bool = False


@dataclass
class Orphan:
    """A process a dead worker left behind, a child of the host now (section 6.10)."""

    pid: int
    left_by: int
    terminate_at: float
    terminated: bool = False
    kill_at: float | None = None
    killed: bool = False


class Host:
    """Preload once, fork workers, and keep them running until told to stop."""

    def __init__(
        self,
        release: Release,
        socket_dir: str,
        workers: int,
        threads: int,
        tcp_base_port: int | None = None,
        sync_threads: int = 8,
        concurrency: int = 1000,
        detach: bool = False,
        groups: list[Group] | None = None,
        mounts: list[Mount] | None = None,
        log_file: str | None = None,
        control: str | None = None,
        max_requests: int | None = None,
        max_lifetime: float | None = None,
        soft_timeout: float | None = None,
        hard_timeout: float | None = None,
        hard_drain_grace: float = 10.0,
        shutdown_timeout: float = 5.0,
        websocket_drain_grace: float = 0.0,
        loop_stall_timeout: float = 10.0,
        gil_stall_timeout: float = 30.0,
        memory: MemoryPolicy | None = None,
        reflection: bool = False,
    ) -> None:
        self.release = release
        self.mounts = mounts or []
        self.reflection = reflection
        self.socket_dir = socket_dir
        self.tcp_base_port = tcp_base_port
        self.detach = detach
        self.log_file = log_file
        self.threads = threads
        self.sync_threads = sync_threads
        self.concurrency = concurrency
        self.max_requests = max_requests
        self.max_lifetime = max_lifetime
        self.soft_timeout = soft_timeout
        self.hard_timeout = hard_timeout
        self.hard_drain_grace = hard_drain_grace
        self.shutdown_timeout = shutdown_timeout
        self.websocket_drain_grace = websocket_drain_grace
        self.loop_stall_timeout = loop_stall_timeout
        self.gil_stall_timeout = gil_stall_timeout
        self.memory = memory or MemoryPolicy()
        self.memory_recycles: dict[int, list[float]] = {}
        self.leak_warned: dict[int, float] = {}

        # Section 6.10: whether orphaned descendants come to this process,
        # and those that have, each with when it is to be stopped.

        self.subreaper = False
        self.orphans: dict[int, Orphan] = {}

        # Workers are numbered across the groups in the order they are
        # forked: the default group first, then each named group, each
        # worker knowing its group and its index within it.

        self.groups = [Group("default", workers, threads, sync_threads, concurrency)]
        self.groups.extend(groups or [])
        self.groups = [self.settled(group) for group in self.groups]
        self.placement: list[tuple[Group, int]] = [
            (group, index) for group in self.groups for index in range(group.processes)
        ]
        self.worker_count = len(self.placement)

        # Section 2.4: a host's workers must not outlive it. Each worker
        # inherits the read end of this pipe and stops when it reaches end
        # of file, which happens only when the host, holding the write
        # end, is gone.

        self.lifeline_read, self.lifeline_write = os.pipe()
        self.workers: dict[int, int] = {}
        self.stopping = False
        self.reopen_requested = False
        self.workers_reopen_requested = False
        self.restart_after: dict[int, float] = {}
        self.spawned_at: dict[int, float] = {}
        self.start_failures: dict[int, int] = {}
        self.failed = False
        self.forked_before = False
        self.selector = selectors.DefaultSelector()
        self.outputs: dict[int, WorkerOutput] = {}

        # Sections 7.6 and 7.7: each worker's state from its reports, the
        # generation of each index's socket (a replacement takes the
        # next), the socket the edge should route each index to (None
        # while a worker is out of routing), and the replacements under
        # way.

        self.states: dict[int, WorkerState] = {}
        self.reports: dict[int, int] = {}

        # Section 11.7: each worker's control connection, by descriptor,
        # with what has arrived on it so far, and the supervisor's
        # requests awaiting answers.

        self.controls: dict[int, int] = {}
        self.control_sockets: dict[int, socket.socket] = {}
        self.control_buffers: dict[int, bytes] = {}
        self.pending: dict[int, Pending] = {}

        # The host's own requests to workers count down from -1, apart
        # from the supervisor's.

        self.own_requests = 0

        # Section 11.7: the reader of the workers' stacks from outside,
        # and the pipe a profile sampled on a thread of its own wakes the
        # loop with.

        self.outside = _outside.Reader()
        self.outside_errors: dict[int, str] = {}
        self.traced: dict[int, list[WorkerState]] = {}
        self.wake_write: int | None = None
        self.generation: dict[int, int] = {}
        self.routed: dict[int, str | None] = {}
        self.replacing: dict[int, Replacement] = {}

        # Section 7.4: indices forked by a scale-out and not yet ready, and
        # indices a scale-in emptied, to be used again by the next one.

        self.starting: set[int] = set()
        self.retired: set[int] = set()

        # Section 5.9: the control socket the supervisor connects to, on
        # which the host announces its worker sockets as they change and
        # the supervisor acknowledges each announcement once the edge
        # routes by it.

        self.control_path = control
        self.control_listener: socket.socket | None = None
        self.supervisor: socket.socket | None = None
        self.supervisor_buffer = b""
        self.version = 0
        self.acked = 0

        if control is not None:
            try:
                os.unlink(control)
            except OSError:
                pass

            self.control_listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.control_listener.bind(control)
            self.control_listener.listen(2)
            self.control_listener.setblocking(False)

    def settled(self, group: Group) -> Group:
        """Section 7.1: the group with the release's values where it set none."""

        return replace(
            group,
            soft_timeout=self.soft_timeout if group.soft_timeout is None else group.soft_timeout,
            hard_timeout=self.hard_timeout if group.hard_timeout is None else group.hard_timeout,
            hard_drain_grace=(
                self.hard_drain_grace if group.hard_drain_grace is None else group.hard_drain_grace
            ),
            loop_stall_timeout=(
                self.loop_stall_timeout
                if group.loop_stall_timeout is None
                else group.loop_stall_timeout
            ),
            gil_stall_timeout=(
                self.gil_stall_timeout
                if group.gil_stall_timeout is None
                else group.gil_stall_timeout
            ),
            memory=self.memory if group.memory is None else group.memory,
        )

    def group_of(self, index: int) -> Group:
        """Return the group a worker index belongs to."""

        return self.placement[index][0]

    @property
    def policies(self) -> bool:
        """Whether any timeout or recycling policy is on."""

        return (
            self.max_requests is not None
            or self.max_lifetime is not None
            or any(
                group.hard_timeout is not None or (group.memory is not None and group.memory.active)
                for group in self.groups
            )
        )

    def socket_path(self, index: int) -> str:
        """Return the Unix socket path, or the TCP address, a worker listens on.

        A named group's sockets carry its name, so the daemon can tell
        them apart; the default group's are numbered plainly. A
        replacement's socket carries its generation, so it never collides
        with the socket of the worker it replaces.
        """

        if self.tcp_base_port is not None:
            return f"127.0.0.1:{self.tcp_base_port + index}"

        group, within = self.placement[index]
        generation = self.generation.get(index, 0)
        suffix = f"-g{generation}" if generation else ""
        if group.name == "default":
            return os.path.join(self.socket_dir, f"worker-{within}{suffix}.sock")

        return os.path.join(self.socket_dir, f"worker-{group.name}-{within}{suffix}.sock")

    def spawn(self, index: int, replacing: bool = False) -> int:
        """Fork one worker and return its pid. The child never returns.

        A worker that is not a replacement is routed to at once; a
        replacement enters routing when its first report arrives.
        """

        if not self.forked_before:
            self.forked_before = True

            if self.release.preloaded and self.release.hooks is not None:
                # Section 6.3: once in the host, before the first fork.

                self.release.hooks.call("pre_fork", self.release.context)

        threads = preload.thread_count()

        if threads > 1:
            log(
                f"forking worker {index} while {threads - 1} other threads are running; "
                "they do not exist in the worker, so start them from a post_fork hook instead"
            )

        # Section 11.2: the worker's stdout and stderr are pipes the host
        # relays with the worker's index, so its lines can be told apart.

        out_read, out_write = os.pipe()
        err_read, err_write = os.pipe()

        # Sections 7.6 and 7.7: the worker reports to the host on a pipe
        # of its own.

        report_read, report_write = os.pipe()
        control_host, control_worker = socket.socketpair()
        path = self.socket_path(index)

        # Section 6.2: freeze before every fork so the collector in the
        # worker never touches the objects it inherited.

        gc.freeze()

        # Anything the host has buffered would otherwise be written twice,
        # once by each process.

        sys.stdout.flush()
        sys.stderr.flush()
        pid = os.fork()

        if pid == 0:
            status = 1

            try:
                os.dup2(out_write, 1)
                os.dup2(err_write, 2)

                # Standard output is now a pipe, which Python block-buffers,
                # so a plain print() would wait for the buffer to fill. Line
                # buffering makes each line arrive as it is written. (The
                # typeshed TextIO has no reconfigure; the real stream does.)

                sys.stdout.reconfigure(line_buffering=True)  # type: ignore[union-attr]
                inherited = (
                    out_read,
                    out_write,
                    err_read,
                    err_write,
                    report_read,
                    *self.outputs,
                    *self.reports,
                    *self.controls,
                )
                for fd in inherited:
                    os.close(fd)
                os.close(self.lifeline_write)
                control_host.close()
                self.close_control()
                status = self.worker(
                    index,
                    lifeline=self.lifeline_read,
                    report=report_write,
                    path=path,
                    control=control_worker.detach(),
                )
            except BaseException:
                traceback.print_exc()
            finally:
                # os._exit skips the flush that a normal exit would do, so
                # whatever is still buffered would be lost.

                for stream in (sys.stdout, sys.stderr):
                    try:
                        stream.flush()
                    except (OSError, ValueError):
                        pass

                os._exit(status)

        os.close(out_write)
        os.close(err_write)
        os.close(report_write)

        for fd, stream in ((out_read, "stdout"), (err_read, "stderr")):
            os.set_blocking(fd, False)
            self.outputs[fd] = WorkerOutput(index, stream, fd)
            self.selector.register(fd, selectors.EVENT_READ)

        os.set_blocking(report_read, False)
        self.reports[report_read] = pid
        self.selector.register(report_read, selectors.EVENT_READ)
        control_worker.close()
        control_host.setblocking(False)
        self.controls[control_host.fileno()] = pid
        self.control_sockets[pid] = control_host
        self.control_buffers[pid] = b""
        self.selector.register(control_host, selectors.EVENT_READ)

        now = time.monotonic()
        state = WorkerState(index, pid, self.generation.get(index, 0), path, now, report_read)
        state.last_report_at = now

        if self.max_requests is not None:
            state.recycle_after_requests = self.max_requests + random.randint(
                0, self.max_requests // 10
            )

        if self.max_lifetime is not None:
            state.recycle_at = now + self.max_lifetime * random.uniform(1.0, 1.1)

        self.states[pid] = state
        self.workers[pid] = index
        self.spawned_at[pid] = now

        if not replacing:
            self.routed[index] = path

        self.emit("worker.started", "info", f"worker {index} started as pid {pid}")
        return pid

    def worker(
        self,
        index: int,
        lifeline: int | None = None,
        report: int | None = None,
        path: str | None = None,
        control: int | None = None,
    ) -> int:
        """Run a worker: enable collection, run the hooks, then serve until the server stops.

        `lifeline` is the read end of the host's pipe, which the server
        watches so the worker stops when the host is gone; `report` the
        write end of the pipe its status lines go to; `path` the socket
        to serve on, else the index's current one; `control` the worker's
        end of its control connection (section 11.7).
        """

        global _in_worker
        _in_worker = True
        gc.enable()

        # Section 11.1: every collection is timed by the runtime, with the
        # generation, what it freed and the requests in flight meanwhile,
        # and reported with the worker's other signals.

        from muru import _native

        gc.callbacks.append(_native.gc_callback)

        # Section 11.7: faulthandler is on in every worker, and SIGQUIT
        # writes every thread's stack to the worker's standard error (the
        # host relays it into the log), which the host uses before it
        # stops a worker past the hard timeout, since no GIL is needed.

        faulthandler.enable(all_threads=True)
        faulthandler.register(signal.SIGQUIT, all_threads=True, chain=False)

        def reclaim() -> None:
            """Section 7.7: give memory back when the host asks (SIGUSR2)."""

            collected = gc.collect()
            trimmed = malloc_trim()
            log(
                f"worker {index} reclaimed memory: collected {collected} objects"
                + (", trimmed the heap" if trimmed else "")
            )

        # The host's handlers and wake-up pipe belong to the host; the native
        # server installs its own handling of SIGTERM, SIGINT and SIGUSR1.

        signal.set_wakeup_fd(-1)

        for signum in (
            signal.SIGTERM,
            signal.SIGINT,
            signal.SIGCHLD,
            signal.SIGUSR1,
            signal.SIGUSR2,
        ):
            signal.signal(signum, signal.SIG_DFL)

        # Section 7.1: MURU_GROUP and the context identify the group after
        # the fork, and the group's sizes are the worker's.

        group, within = self.placement[index]
        os.environ["MURU_GROUP"] = group.name
        release = self.release
        context = replace(release.context, worker_id=within, group=group.name)
        set_context(context)

        loop: _asgi.Loop | None = None
        lifespan: _asgi.Lifespan | None = None

        # Section 6.8: an interface not settled in the host, because the
        # workers import the application or a factory makes it, is
        # settled here, and each worker says what it serves.

        known = release.resolved is not None
        known_mounts = {mount.name for mount in self.mounts if mount.release.resolved is not None}

        # ADR 0033: a group that lists mounts serves those and the default
        # mount; the host imported every mount, so the others cost this
        # worker nothing after the fork.

        mounts = [mount for mount in self.mounts if not group.mounts or mount.name in group.mounts]

        try:
            if release.preloaded:
                hooks = release.hooks or Hooks()
            else:
                # Per-worker imports: everything the host would have done.

                started = time.monotonic()
                hooks = release.load()
                for mount in mounts:
                    mount.release.load()
                log(f"worker {index} imported {release.spec} in {elapsed_ms(started)} ms")
                hooks.call("on_load", context)
                hooks.call("pre_fork", context)

            hooks.call("post_fork", context)

            # Section 6.8: a factory is called per worker, after post_fork,
            # and only then is the interface known for certain.

            app = release.realise()
            context = replace(release.context, worker_id=within, group=group.name)
            set_context(context)

            if not known and release.resolved is not None:
                log(f"worker {index} serves {release.spec} as {interface_name(release.resolved)}")

            # Section 6.7: each mount's application is realised like the
            # release's own; section 6.6: one event loop per worker when
            # any of them is ASGI, started after the fork, with lifespan
            # startup for each ASGI application before the worker is ready.

            realised = [
                (mount.name, mount.release.realise(), mount.release, mount.threads)
                for mount in mounts
            ]
            limits = {mount.name: mount.limits for mount in mounts}

            for name, _, mounted, _ in realised:
                if name not in known_mounts and mounted.resolved is not None:
                    log(
                        f"worker {index} serves mount {name}: {mounted.spec} as "
                        f"{interface_name(mounted.resolved)}"
                    )

            any_asgi = release.resolved in ("asgi", "grpc") or any(
                mounted.resolved in ("asgi", "grpc") for _, _, mounted, _ in realised
            )

            for name, _, mounted, own in realised:
                if own is not None and mounted.resolved == "asgi":
                    raise ValueError(
                        f"mount {name}: an ASGI mount runs on the event loop and has no threads"
                    )

            # Section 6.5: the shared pool serves a WSGI default mount and
            # every WSGI mount that has no threads of its own or borrows.

            shared_threads = (
                group.threads
                if release.resolved not in ("asgi", "grpc")
                or any(
                    mounted.resolved not in ("asgi", "grpc") and (own is None or own[0] < own[1])
                    for _, _, mounted, own in realised
                )
                else 1
            )

            if any_asgi:
                loop = _asgi.Loop(group.sync_threads)
                loop.start()

            if loop is not None and release.resolved == "asgi":
                lifespan = _asgi.Lifespan(app, loop, release.spec)
                lifespan.startup()

            lifespans: list[_asgi.Lifespan] = []
            served_mounts: list[
                tuple[str, Any, Any, tuple[int, int] | None, tuple[int, int] | None]
            ] = []

            # Section 6.13: a gRPC application is served by a dispatcher on
            # the loop, which runs synchronous handlers on threads of the
            # mount's own, and its methods are declared to the edge.

            grpc_servers: list[tuple[str, Any]] = []
            grpc_runner: Any = None

            if loop is not None and release.resolved == "grpc":
                from muru import _grpc

                grpc_servers.append(("", app))
                app = _grpc.Dispatcher(app, group.threads)
                grpc_runner = _asgi.runner(app, loop, {})

            for name, mounted_app, mounted, own in realised:
                if loop is not None and mounted.resolved == "grpc":
                    from muru import _grpc

                    grpc_servers.append((name, mounted_app))
                    dispatcher = _grpc.Dispatcher(
                        mounted_app, group.threads if own is None else own[0]
                    )
                    runner = _asgi.runner(dispatcher, loop, {})
                    served_mounts.append((name, dispatcher, runner, own, None))
                elif loop is not None and mounted.resolved == "asgi":
                    mounted_lifespan = _asgi.Lifespan(mounted_app, loop, mounted.spec)
                    mounted_lifespan.startup()
                    lifespans.append(mounted_lifespan)
                    runner = _asgi.runner(mounted_app, loop, mounted_lifespan.state)
                    served_mounts.append((name, mounted_app, runner, None, limits[name]))
                else:
                    served_mounts.append((name, mounted_app, None, own, None))

            if grpc_servers and self.reflection:
                from muru import _grpc

                _grpc.enable_reflection([server for _, server in grpc_servers])

            grpc_methods = [
                method for name, server in grpc_servers for method in server.listing(name)
            ]

            hooks.call("on_ready", context)
        except Exception as error:
            log(f"worker {index} failed to start: {error.__class__.__name__}: {error}")
            traceback.print_exc()
            if loop is not None:
                loop.stop()
            return 1

        from muru import _native

        def on_exit() -> None:
            """Run lifespan shutdown for every ASGI application and the hook, then stop the loop."""

            for mounted_lifespan in lifespans:
                mounted_lifespan.shutdown()
            if lifespan is not None:
                lifespan.shutdown()
            hooks.call_logged("on_exit", context)
            if loop is not None:
                loop.stop()

        # A worker with any ASGI application hands the extension its loop,
        # and the request runner when the default mount is ASGI.

        asgi_arguments: dict[str, Any] = {}

        if loop is not None:
            asgi_arguments = {"event_loop": loop.loop, "concurrency": group.concurrency}

        if loop is not None and lifespan is not None:
            asgi_arguments["runner"] = _asgi.runner(app, loop, lifespan.state)

        if grpc_runner is not None:
            asgi_arguments["runner"] = grpc_runner

        # Section 7.6: the soft timeout interrupts a request by raising
        # muru.RequestTimeout in its thread, or cancelling its task.

        # Told to stop, a worker keeps its open WebSockets for their grace,
        # then closes them with 1001 and gives what is left the shutdown
        # timeout, before the host kills it.

        timeout_arguments: dict[str, Any] = {
            "websocket_drain_ms": int(self.websocket_drain_grace * 1000),
            "drain_ms": int(self.shutdown_timeout * 1000),
        }

        if group.soft_timeout is not None:
            timeout_arguments["soft_timeout_ms"] = int(group.soft_timeout * 1000)
            timeout_arguments["timeout_error"] = RequestTimeout

        # Section 7.6: the event loop beats twice a second, and a native
        # watchdog tells the host when it has stopped beating for longer
        # than the loop stall timeout.

        if loop is not None:
            timeout_arguments["loop_stall_timeout_ms"] = int(
                (group.loop_stall_timeout or self.loop_stall_timeout) * 1000
            )
            _asgi.heartbeat(loop.loop)

        # Section 7.6: a thread attaches to the interpreter every half
        # second, and a native watchdog tells the host when none has for
        # longer than the GIL stall timeout.

        timeout_arguments["gil_stall_timeout_ms"] = int(
            (group.gil_stall_timeout or self.gil_stall_timeout) * 1000
        )

        # Section 11.7: breakpoint() waits for muructl debug attach.

        from muru import _debug

        _debug.install(index, report)

        try:
            _native.serve(
                path or self.socket_path(index),
                app,
                shared_threads,
                # Section 6.5: False only for a single-process development run.
                multiprocess=self.worker_count > 1,
                on_drain=lambda: hooks.call_logged("on_drain", context),
                on_exit=on_exit,
                on_reopen_logs=lambda: hooks.call_logged("on_reopen_logs", context),
                lifeline=lifeline,
                mounts=served_mounts,
                report=report,
                on_reclaim=reclaim,
                memory_interval_ms=int((group.memory or self.memory).check_interval * 1000),
                control=control,
                on_control=lambda line, active, names: _inspect.handle(
                    line,
                    active,
                    loop.loop if loop is not None else None,
                    names,
                    slots=group.threads if loop is None else group.concurrency,
                ),
                grpc_methods=grpc_methods,
                **asgi_arguments,
                **timeout_arguments,
            )
        except Exception as error:
            log(f"worker {index} failed: {error}")
            return 1

        return 0

    def run(self) -> int:
        """Supervise the workers until shutdown."""

        selector = self.selector
        wake_read, wake_write = os.pipe()
        os.set_blocking(wake_write, False)
        signal.set_wakeup_fd(wake_write, warn_on_full_buffer=False)
        selector.register(wake_read, selectors.EVENT_READ)
        self.wake_write = wake_write

        def request_stop(signum: int, _frame: Any) -> None:
            # Ctrl-C sends SIGINT and muructl SIGTERM close together, and
            # the second handler may run while the first is still logging,
            # so the flag is set before the line is written.

            first = not self.stopping
            self.stopping = True

            if first:
                log(f"received {signal.Signals(signum).name}, stopping")

        def request_reopen(_signum: int, _frame: Any) -> None:
            self.reopen_requested = True

        def request_workers_reopen(_signum: int, _frame: Any) -> None:
            self.workers_reopen_requested = True

        signal.signal(signal.SIGTERM, request_stop)
        signal.signal(signal.SIGINT, request_stop)
        signal.signal(signal.SIGCHLD, lambda _signum, _frame: None)
        signal.signal(signal.SIGUSR1, request_reopen)
        signal.signal(signal.SIGUSR2, request_workers_reopen)

        # Section 13.3: a host under the daemon outlives it, so the daemon
        # can be restarted or upgraded and re-adopt it; one under muructl
        # run stops when its parent's end of stdin closes.

        watch_stdin = False

        try:
            if not self.detach and not os.isatty(0):
                selector.register(0, selectors.EVENT_READ)
                watch_stdin = True
        except OSError:
            pass

        if self.control_listener is not None:
            selector.register(self.control_listener, selectors.EVENT_READ)

        # Section 6.10: a subreaper, so what a worker leaves running comes
        # back to the host to be stopped and collected, rather than to init.

        self.subreaper = become_subreaper()

        if not self.subreaper and os.uname().sysname == "Linux":
            log("could not become a subreaper; processes the workers leave behind go to init")

        # An optional group's workers are routed to once they listen, as
        # a scale-out's are, so the edge holds the group's requests until
        # one does rather than sending them to a socket nobody serves.

        for index in range(self.worker_count):
            if self.group_of(index).optional:
                self.starting.add(index)
                self.spawn(index, replacing=True)
            else:
                self.spawn(index)

        self.announce()

        while True:
            if self.restart_after:
                timeout: float | None = 0.1
            elif self.policies or self.replacing or self.pending or self.states or self.orphans:
                timeout = TICK_SECONDS
            else:
                timeout = None

            for key, _events in selector.select(timeout):
                if key.fd == wake_read:
                    os.read(wake_read, 4096)
                elif watch_stdin and key.fd == 0:
                    if os.read(0, 4096) == b"":
                        if not self.stopping:
                            log("parent went away, stopping")
                        self.stopping = True
                        selector.unregister(0)
                        watch_stdin = False
                elif key.fd in self.outputs:
                    self.read_output(key.fd)
                elif key.fd in self.reports:
                    self.read_report(key.fd)
                elif key.fd in self.controls:
                    self.read_worker_control(key.fd)
                elif self.control_listener is not None and key.fileobj is self.control_listener:
                    self.accept_supervisor()
                elif self.supervisor is not None and key.fileobj is self.supervisor:
                    self.read_supervisor()

            self.reap()
            self.reopen_if_requested()
            if self.stopping:
                break
            self.restart_due()
            self.tick()

        self.terminate_all()
        for fd in list(self.outputs):
            self.read_output(fd, until_closed=True)
        signal.set_wakeup_fd(-1)
        return 1 if self.failed else 0

    # Section 5.9: the control connection to the supervisor.

    def accept_supervisor(self) -> None:
        """Take a new supervisor connection and tell it the worker sockets.

        A new connection replaces any earlier one.
        """

        assert self.control_listener is not None

        try:
            connection, _ = self.control_listener.accept()
        except OSError:
            return

        self.drop_supervisor()
        connection.setblocking(False)
        self.supervisor = connection
        self.supervisor_buffer = b""
        self.selector.register(connection, selectors.EVENT_READ)
        self.send_to_supervisor(self.workers_message())

    def drop_supervisor(self) -> None:
        """Close the supervisor connection, if there is one."""

        if self.supervisor is None:
            return

        try:
            self.selector.unregister(self.supervisor)
        except (KeyError, ValueError):
            pass

        self.supervisor.close()
        self.supervisor = None

    def close_control(self) -> None:
        """Close the control sockets, in a worker after fork."""

        if self.control_listener is not None:
            self.control_listener.close()

        if self.supervisor is not None:
            self.supervisor.close()

    def read_supervisor(self) -> None:
        """Read the supervisor's acknowledgements."""

        assert self.supervisor is not None

        try:
            data = self.supervisor.recv(READ_SIZE)
        except BlockingIOError:
            return
        except OSError:
            data = b""

        if not data:
            self.drop_supervisor()
            return

        self.supervisor_buffer += data

        while b"\n" in self.supervisor_buffer:
            line, _, self.supervisor_buffer = self.supervisor_buffer.partition(b"\n")

            try:
                message = json.loads(line)
            except ValueError:
                continue

            if message.get("type") == "applied":
                self.acked = max(self.acked, int(message.get("version", 0)))
            elif message.get("type") == "request":
                self.take_request(message)

    def send_to_supervisor(self, message: dict[str, Any]) -> None:
        """Send one message as a JSON line, dropping the connection if that fails."""

        if self.supervisor is None:
            return

        # Blocking for the send, with a bound, so an answer larger than
        # the socket's buffer (an inspect of many modules) gets through.

        try:
            self.supervisor.settimeout(5.0)
            self.supervisor.sendall(json.dumps(message).encode() + b"\n")
        except OSError:
            self.drop_supervisor()
            return

        self.supervisor.setblocking(False)

    def workers_message(self) -> dict[str, Any]:
        """Build the announcement of the sockets the edge should route to, by group."""

        by_group: dict[str, list[str]] = {}

        for index, (placed, _within) in enumerate(self.placement):
            path = self.routed.get(index)

            if path is not None:
                by_group.setdefault(placed.name, []).append(path)

        return {
            "type": "workers",
            "version": self.version,
            "sockets": by_group.get("default", []),
            "groups": [
                {"name": group.name, "sockets": by_group.get(group.name, [])}
                for group in self.groups
                if group.name != "default"
            ],
            "interfaces": self.interfaces(),
        }

    def interfaces(self) -> dict[str, str]:
        """Return the interfaces settled in the host, by mount, the default as ``""``.

        One that each worker settles, importing the application itself or
        calling a factory, is not known here and is left out.
        """

        settled = {"": self.release.resolved} if self.release.resolved else {}

        for mount in self.mounts:
            if mount.release.resolved:
                settled[mount.name] = mount.release.resolved

        return settled

    def announce(self) -> int:
        """Tell the supervisor the worker sockets changed; return the version."""

        self.version += 1
        self.send_to_supervisor(self.workers_message())
        return self.version

    def emit(
        self,
        kind: str,
        level: str,
        message: str,
        group: str | None = None,
        reason: str | None = None,
    ) -> None:
        """Log a worker event and pass it to the supervisor.

        `group` and `reason` are the worker's group and the event's reason
        as one word, which the supervisor counts (section 11.1); the
        message says the same in words.
        """

        log(message)
        event = {"type": "event", "kind": kind, "level": level, "message": message}

        if group is not None:
            event["group"] = group

        if reason is not None:
            event["reason"] = reason

        self.send_to_supervisor(event)

    # Section 11.7: the supervisor's questions, answered by the host or
    # passed to the workers.

    def take_request(self, message: dict[str, Any]) -> None:
        """Answer a request from the host's own state or ask the workers."""

        request_id = int(message.get("id", 0))
        op = str(message.get("op", ""))
        wanted = message.get("worker")
        args = message.get("args") or {}
        targets = {
            state.pid: state.index
            for state in self.states.values()
            if state.ready and not state.replaced and (wanted is None or state.index == wanted)
        }

        if op == "scale":
            self.scale_group(request_id, args)
            return

        if op == "restart":
            self.restart_worker(request_id, wanted, args)
            return

        if op == "status":
            self.send_to_supervisor(
                {
                    "type": "response",
                    "id": request_id,
                    "workers": [
                        self.status_of(state)
                        for state in sorted(self.states.values(), key=lambda s: s.index)
                        if wanted is None or state.index == wanted
                    ],
                }
            )
            return

        if not targets:
            self.send_to_supervisor(
                {"type": "response", "id": request_id, "workers": [], "error": "no such worker"}
            )
            return

        # Section 11.7: stacks, and a profile of everything a worker runs,
        # are read from outside where the interpreter allows it, so they
        # work on a worker whose threads hold the interpreter; elsewhere
        # the worker is asked.

        if op == "stacks" and self.stacks_from_outside(request_id, targets):
            return

        chosen = args.get("requests") or args.get("path")

        if op == "profile" and not chosen and self.profile_from_outside(request_id, targets, args):
            return

        if op == "trace":
            self.trace_started(request_id, targets)

        pending = Pending(request_id, op, targets)
        pending.deadline = time.monotonic() + float(args.get("timeout_s", CONTROL_TIMEOUT_SECONDS))
        self.pending[request_id] = pending
        line = json.dumps({"id": request_id, "op": op, "args": args}).encode() + b"\n"

        for pid, index in list(targets.items()):
            control = self.control_sockets.get(pid)

            try:
                if control is None:
                    raise OSError("no control connection")

                control.sendall(line)
            except OSError as error:
                pending.answers[pid] = {
                    "index": index,
                    "pid": pid,
                    "ok": False,
                    "error": f"could not be asked: {error}",
                }

        self.finish_if_answered(pending)

    # Section 7.4: the supervisor's scaling of one group, carried out by
    # forking workers at new indices or draining the group's youngest; a
    # group scaled to zero keeps the host, which forks again when asked.

    def scale_group(self, request_id: int, args: dict[str, Any]) -> None:
        """Bring a group to the worker count the supervisor asks for."""

        name = str(args.get("group", "default"))
        wanted = int(args.get("processes", 0))
        group = next((candidate for candidate in self.groups if candidate.name == name), None)
        error = ""

        if group is None:
            error = f"no worker group {name}"
        elif wanted < 0:
            error = "processes must be at least 0"

        if error:
            self.send_to_supervisor(
                {"type": "response", "id": request_id, "workers": [], "error": error}
            )
            return

        assert group is not None
        current = len(self.active_indices(group))

        for _ in range(current, wanted):
            self.add_worker(group)

        for _ in range(wanted, current):
            self.retire_worker(group)

        self.send_to_supervisor({"type": "response", "id": request_id, "workers": []})

    # Section 7.6: a worker replaced because the operator asked, through
    # the path the policies use; with force, killed at once.

    def restart_worker(self, request_id: int, wanted: Any, args: dict[str, Any]) -> None:
        """Replace one worker on request, gracefully or at once with force."""

        force = bool(args.get("force"))
        state = next(
            (
                candidate
                for candidate in self.states.values()
                if candidate.index == wanted and not candidate.replaced
            ),
            None,
        )
        error = ""

        if wanted is None:
            error = "a worker index is needed"
        elif state is None:
            error = f"no worker {wanted}"
        elif not state.ready:
            error = f"worker {wanted} is still starting"
        elif wanted in self.replacing:
            error = f"worker {wanted} is already being replaced"
        elif state.debug_sessions and not force:
            error = f"worker {wanted} is being debugged; --force ends the session"
        elif self.start_failures.get(wanted, 0) >= MAX_START_FAILURES:
            error = (
                f"a replacement for worker {wanted} failed to start {MAX_START_FAILURES} "
                "times and is not tried again"
            )
        elif self.control_listener is None:
            error = "workers cannot be replaced without a control socket"

        if error or state is None:
            self.send_to_supervisor(
                {"type": "response", "id": request_id, "workers": [], "error": error}
            )
            return

        if force:
            state.debug_sessions = 0
            self.replace(
                state, "restarted on request, killed at once", "worker.restarted", hard=True
            )
        else:
            self.replace(state, "restarted on request", "worker.restarted")

        replacement = self.replacing[state.index]

        # A forced restart takes the stacks the way a hard timeout does,
        # then kills the worker without asking it to stop.

        if force:
            self.capture_stacks(state, replacement)
            replacement.terminated = True
            replacement.kill_at = time.monotonic() + FORCE_KILL_SECONDS

        self.send_to_supervisor(
            {
                "type": "response",
                "id": request_id,
                "workers": [
                    {
                        "index": state.index,
                        "pid": state.pid,
                        "ok": True,
                        "result": {
                            "old_pid": state.pid,
                            "new_pid": replacement.new_pid,
                            "forced": force,
                        },
                    }
                ],
            }
        )

    def active_indices(self, group: Group) -> list[int]:
        """Return the group's indices with a worker that serves or is starting, in order."""

        return [
            index
            for index, (placed, _within) in enumerate(self.placement)
            if placed is group
            and index not in self.retired
            and not (index in self.replacing and self.replacing[index].retire)
        ]

    def add_worker(self, group: Group) -> int:
        """Fork one more worker for the group; it enters routing once ready.

        A retired index of the group is used again, on a socket of the
        next generation so it never collides with the old worker's.
        """

        retired = sorted(index for index in self.retired if self.placement[index][0] is group)

        if retired:
            index = retired[0]
            self.retired.discard(index)
            self.generation[index] = self.generation.get(index, 0) + 1
        else:
            index = len(self.placement)
            within = sum(1 for placed, _within in self.placement if placed is group)
            self.placement.append((group, within))
            self.worker_count = len(self.placement)

        self.starting.add(index)
        self.spawn(index, replacing=True)
        return index

    def retire_worker(self, group: Group) -> None:
        """Take the group's youngest worker out of routing and then stop it."""

        candidates = [
            state
            for state in self.states.values()
            if state.ready
            and not state.replaced
            and not state.debug_sessions
            and state.index not in self.replacing
            and self.placement[state.index][0] is group
        ]

        if not candidates:
            return

        state = max(candidates, key=lambda candidate: candidate.started_at)
        state.replaced = True
        retirement = Replacement(
            state.index, state.pid, "scaled in", "worker.exited", "scaled_in", retire=True
        )
        self.replacing[state.index] = retirement
        self.routed[state.index] = None
        retirement.announced = self.announce()
        retirement.announced_at = time.monotonic()
        log(f"worker {state.index} (pid {state.pid}) leaves routing: scaled in")

    def status_of(self, state: WorkerState) -> dict[str, Any]:
        """One worker's state as the host knows it.

        A worker is routed once the host's latest announcement names it
        and the supervisor has acknowledged that announcement, so the edge
        routes to it; a replacement still warming, or a worker out of
        routing while it is traced or debugged, is not.
        """

        return {
            "index": state.index,
            "pid": state.pid,
            "ok": True,
            "result": {
                "group": self.placement[state.index][0].name,
                "generation": state.generation,
                "socket": state.path,
                "ready": state.ready,
                "replaced": state.replaced,
                "routed": self.routed.get(state.index) == state.path and self.acked >= self.version,
                "debugging": state.debug_sessions > 0,
                "uptime_s": round(time.monotonic() - state.started_at, 1),
                "requests": state.requests,
                "in_flight": state.in_flight,
                "oldest_ms": state.oldest_ms,
                **state.memory,
            },
        }

    def read_worker_control(self, fd: int) -> None:
        """Take a worker's answers; drop the connection at end of file."""

        pid = self.controls[fd]
        control = self.control_sockets.get(pid)

        if control is None:
            return

        try:
            data = control.recv(READ_SIZE)
        except BlockingIOError:
            return
        except OSError:
            data = b""

        if not data:
            self.selector.unregister(control)
            control.close()
            del self.controls[fd]
            del self.control_sockets[pid]
            self.control_buffers.pop(pid, None)
            return

        self.control_buffers[pid] = self.control_buffers.get(pid, b"") + data

        while b"\n" in self.control_buffers[pid]:
            line, _, self.control_buffers[pid] = self.control_buffers[pid].partition(b"\n")

            try:
                answer = json.loads(line)
            except ValueError:
                continue

            # An answer without an id (the runtime could not run the
            # handler) is for the oldest request this worker still owes.

            if answer.get("id") is None:
                pending = next(
                    (
                        candidate
                        for candidate in sorted(self.pending.values(), key=lambda p: p.id)
                        if pid in candidate.expected and pid not in candidate.answers
                    ),
                    None,
                )
            else:
                pending = self.pending.get(int(answer["id"]))

            if pending is None or pid not in pending.expected:
                continue

            pending.answers[pid] = {
                "index": pending.expected[pid],
                "pid": pid,
                "ok": bool(answer.get("ok")),
                **({"result": answer["result"]} if "result" in answer else {}),
                **({"error": answer["error"]} if "error" in answer else {}),
            }
            self.finish_if_answered(pending)

    def finish_if_answered(self, pending: Pending, expired: bool = False) -> None:
        """Send the answers on once every worker has one, or the time is up."""

        if not expired and len(pending.answers) < len(pending.expected):
            return

        for pid, index in pending.expected.items():
            if pid in pending.answers:
                continue

            # A worker that cannot be read is at least made to write its
            # stacks to the log, which needs no interpreter.

            note = ""

            if pending.op == "stacks":
                self.signal(pid, signal.SIGQUIT)
                note = "; its stacks were written to the log by faulthandler"

            pending.answers[pid] = {
                "index": index,
                "pid": pid,
                "ok": False,
                "error": f"did not answer in {CONTROL_TIMEOUT_SECONDS:g} s{note}",
            }

        self.pending.pop(pending.id, None)

        if pending.op == "trace":
            self.trace_ended(pending.id)

        if pending.op == "attach":
            for pid, answer in pending.answers.items():
                state = self.states.get(pid)

                if state is not None and answer.get("ok"):
                    self.debugging(state, 1)

        if pending.then is not None:
            pending.then(pending)
            return

        self.send_to_supervisor(
            {
                "type": "response",
                "id": pending.id,
                "workers": sorted(pending.answers.values(), key=lambda answer: answer["index"]),
            }
        )

    def trace_started(self, request_id: int, targets: dict[int, int]) -> None:
        """Take traced workers out of routing while their trace lasts.

        Section 11.7: tracing slows a worker, so the edge's requests go to
        the others of its group meanwhile. A worker stays in routing when
        it is the last of its group that is, and finishes the requests it
        has either way.
        """

        taken = []

        for pid in targets:
            state = self.states.get(pid)

            if state is None or self.routed.get(state.index) is None:
                continue

            group = self.placement[state.index][0]
            others = [
                other
                for other in self.states.values()
                if other.pid != pid
                and self.placement[other.index][0] is group
                and self.routed.get(other.index) is not None
            ]

            if not others:
                continue

            self.routed[state.index] = None
            taken.append(state)
            log(f"worker {state.index} (pid {state.pid}) leaves routing while it is traced")

        if taken:
            self.traced[request_id] = taken
            self.announce()

    def trace_ended(self, request_id: int) -> None:
        """Put workers back in routing once their trace has answered."""

        returned = False

        for state in self.traced.pop(request_id, []):
            if state.replaced or state.debug_sessions or state.pid not in self.states:
                continue

            self.routed[state.index] = state.path
            returned = True
            log(f"worker {state.index} (pid {state.pid}) serves again: its trace has ended")

        if returned:
            self.announce()

    def debugging(self, state: WorkerState, change: int) -> None:
        """Count a debugger session opening or ending in a worker.

        Section 11.7: while any is open the worker is out of routing and
        pinned, so no policy replaces it; it serves again after the last.
        """

        before = state.debug_sessions
        state.debug_sessions = max(0, before + change)

        if state.replaced or (before > 0) == (state.debug_sessions > 0):
            return

        if state.debug_sessions:
            self.routed[state.index] = None
            log(f"worker {state.index} (pid {state.pid}) leaves routing while it is debugged")
        else:
            self.routed[state.index] = state.path
            log(f"worker {state.index} (pid {state.pid}) serves again: no debugger is attached")

        self.announce()

    # Sections 7.6 and 7.7: reports, policies and replacements.

    def read_report(self, fd: int) -> None:
        """Take a worker's status lines; close the pipe at end of file."""

        pid = self.reports[fd]
        state = self.states.get(pid)

        try:
            data = os.read(fd, READ_SIZE)
        except BlockingIOError:
            return
        except OSError:
            data = b""

        if not data:
            self.selector.unregister(fd)
            os.close(fd)
            del self.reports[fd]
            return

        if state is None:
            return

        measured = False

        for line in data.splitlines():
            try:
                report = json.loads(line)
            except ValueError:
                continue

            # Section 11.7: an event the worker raises, such as a request
            # past the soft timeout with where it was.

            if "event" in report:
                self.emit(
                    str(report["event"]),
                    str(report.get("level", "warning")),
                    f"worker {state.index} (pid {state.pid}): {report.get('message', '')}",
                )

                if report["event"] == "debug.detached":
                    self.debugging(state, -1)
                elif report["event"] == "worker.loop_stalled":
                    loop_stall = self.group_of(state.index).loop_stall_timeout
                    self.replace(
                        state,
                        f"its event loop stalled past the loop stall timeout of {loop_stall:g} s",
                        "worker.timeout_killed",
                        "loop_stalled",
                        abandon={
                            "which": "all",
                            "reason": "worker_stalled",
                            "error": f"the worker's event loop stalled past the loop stall "
                            f"timeout of {loop_stall:g} s",
                        },
                    )
                elif report["event"] == "worker.gil_stalled":
                    gil_stall = self.group_of(state.index).gil_stall_timeout
                    self.replace(
                        state,
                        f"no thread of it ran Python for the GIL stall timeout of {gil_stall:g} s",
                        "worker.timeout_killed",
                        "gil_stalled",
                        abandon={
                            "which": "all",
                            "reason": "worker_stalled",
                            "error": f"no thread of the worker ran Python for the GIL stall "
                            f"timeout of {gil_stall:g} s",
                        },
                    )

                continue

            state.last_report_at = time.monotonic()
            state.requests = int(report.get("requests", state.requests))
            state.in_flight = int(report.get("in_flight", state.in_flight))
            state.oldest_ms = int(report.get("oldest_ms", state.oldest_ms))

            if "rss" in report:
                measured = True
                state.memory = {
                    key: int(report[key]) for key in ("rss", "pss", "uss") if key in report
                }

            if "cpu_ms" in report:
                measured = True
                state.signals = {
                    key: report[key]
                    for key in (
                        "cpu_ms",
                        "gil_enabled",
                        "gil_wait",
                        "gil_wait_us",
                        "loop_lag",
                        "loop_lag_us",
                        "gc",
                    )
                    if key in report
                }

        if not state.ready:
            state.ready = True
            self.worker_ready(state)

        self.check_requests(state)

        if measured:
            self.check_memory(state)
            self.send_status()

    def check_memory(self, state: WorkerState) -> None:
        """Section 7.7: hold a worker to the memory limits."""

        policy = self.group_of(state.index).memory or self.memory

        if state.replaced or state.index in self.replacing or not policy.active:
            return

        value = state.memory.get(policy.measure)
        if value is None:
            return

        now = time.monotonic()
        hard = policy.hard_limit
        soft = policy.soft_limit

        if hard is not None and value > hard:
            self.replace(
                state,
                f"{policy.measure} {format_size(value)} is over the hard limit of "
                f"{format_size(hard)}",
                "worker.recycled",
                "memory",
                hard=True,
            )
            return

        if soft is None or value <= soft:
            state.over_soft_since = None
            state.reclaimed_at = None
            return

        if state.over_soft_since is None:
            state.over_soft_since = now
            return

        if now - state.over_soft_since < policy.soft_for:
            return

        # Over the soft limit for long enough: reclaim first, then recycle
        # when the next measurement is still over.

        if policy.reclaim_first and state.reclaimed_at is None:
            state.reclaimed_at = now
            self.signal(state.pid, signal.SIGUSR2)
            return

        self.replace(
            state,
            f"{policy.measure} {format_size(value)} stayed over the soft limit of "
            f"{format_size(soft)} for {now - state.over_soft_since:.0f} s"
            + (" after reclaiming" if state.reclaimed_at is not None else ""),
            "worker.recycled",
            "memory",
        )

    def note_memory_recycle(self, index: int) -> None:
        """Warn when one worker index keeps recycling for memory."""

        now = time.monotonic()
        recent = [
            at for at in self.memory_recycles.get(index, []) if now - at < LEAK_WINDOW_SECONDS
        ]
        recent.append(now)
        self.memory_recycles[index] = recent

        if (
            len(recent) >= LEAK_RECYCLES
            and now - self.leak_warned.get(index, -LEAK_WINDOW_SECONDS) >= LEAK_WINDOW_SECONDS
        ):
            self.leak_warned[index] = now
            self.emit(
                "memory.leak_suspected",
                "warning",
                f"worker {index} was recycled for memory {len(recent)} times in the last hour",
            )

    def send_status(self) -> None:
        """Tell the supervisor what every worker last reported."""

        self.send_to_supervisor(
            {
                "type": "status",
                "workers": [
                    {
                        "index": state.index,
                        "pid": state.pid,
                        "group": self.placement[state.index][0].name,
                        "requests": state.requests,
                        "in_flight": state.in_flight,
                        "oldest_ms": state.oldest_ms,
                        **state.memory,
                        **state.signals,
                    }
                    for state in sorted(self.states.values(), key=lambda state: state.index)
                    if not state.replaced
                ],
            }
        )

    def worker_ready(self, state: WorkerState) -> None:
        """Note that a worker listens; a replacement enters routing and is announced."""

        self.emit("worker.ready", "info", f"worker {state.index} (pid {state.pid}) is ready")
        self.start_failures[state.index] = 0
        replacement = self.replacing.get(state.index)

        if replacement is not None and replacement.new_pid == state.pid:
            self.routed[state.index] = state.path
            replacement.announced = self.announce()
            replacement.announced_at = time.monotonic()
        elif state.index in self.starting:
            self.starting.discard(state.index)
            self.routed[state.index] = state.path
            self.announce()

    def check_requests(self, state: WorkerState) -> None:
        """Replace a worker that has served its share of requests."""

        if state.replaced or state.index in self.replacing:
            return

        if (
            state.recycle_after_requests is not None
            and state.requests >= state.recycle_after_requests
        ):
            self.replace(
                state,
                f"served {state.requests} requests (max_requests {self.max_requests})",
                "worker.recycled",
                "max_requests",
            )

    def tick(self) -> None:
        """Apply the lifetime and hard timeout policies and advance replacements."""

        now = time.monotonic()
        self.advance_orphans(now)

        for state in list(self.states.values()):
            if state.replaced or state.index in self.replacing or not state.ready:
                continue

            group = self.group_of(state.index)
            hard_timeout = group.hard_timeout
            gil_stall_timeout = group.gil_stall_timeout or self.gil_stall_timeout

            if hard_timeout is not None and state.oldest_ms > hard_timeout * 1000:
                self.replace(
                    state,
                    f"a request has run for {state.oldest_ms / 1000:.1f} s, past the hard "
                    f"timeout of {hard_timeout:g} s",
                    "worker.timeout_killed",
                    "hard_timeout",
                    abandon={
                        "which": "oldest",
                        "reason": "hard_timeout",
                        "error": f"the request ran for {state.oldest_ms / 1000:.1f} s, past "
                        f"the hard timeout of {hard_timeout:g} s",
                    },
                )
            elif now - state.last_report_at > gil_stall_timeout + REPORT_INTERVAL_SECONDS:
                # Section 7.6: a worker whose reports have stopped, which
                # its native threads send, is stopped or frozen whole, so
                # none of its requests can finish. The silence is counted
                # from when the next report was due, as a report interval
                # of jitter is not a stall.

                silent = now - state.last_report_at
                self.replace(
                    state,
                    f"sent no report for {silent:.0f} s, longer than "
                    f"the GIL stall timeout of {gil_stall_timeout:g} s",
                    "worker.timeout_killed",
                    "gil_stalled",
                    abandon={
                        "which": "all",
                        "reason": "worker_stalled",
                        "error": f"the worker sent no report for {silent:.0f} s, longer than "
                        f"the GIL stall timeout of {gil_stall_timeout:g} s",
                    },
                )
            elif state.recycle_at is not None and now >= state.recycle_at:
                self.replace(
                    state,
                    f"lived {now - state.started_at:.0f} s (max_lifetime {self.max_lifetime:g} s)",
                    "worker.recycled",
                    "max_lifetime",
                )

        for replacement in list(self.replacing.values()):
            self.advance(replacement, now)

        # A profile sampled from outside is answered by its thread, so it is
        # looked at here as well as when its time is up.

        for pending in list(self.pending.values()):
            self.finish_if_answered(pending, expired=now >= pending.deadline)

    def replace(
        self,
        state: WorkerState,
        reason: str,
        kind: str,
        cause: str = "",
        hard: bool = False,
        abandon: dict[str, str] | None = None,
    ) -> None:
        """Start replacing a worker with a fresh one on a new socket.

        `cause` is the reason as one word, for the supervisor's counters.
        For a hard timeout the old worker leaves routing at once and its
        stacks are recorded, and `abandon` says which of its requests the
        edge answers now rather than after the grace: the oldest, which is
        the stuck one, or all of them when the whole worker is stalled. A
        worker being debugged is pinned: the policy applies once the last
        session ends.
        """

        if state.debug_sessions:
            return

        # A worker condemned twice, by the silence check and then by its
        # own stall event, or by the hard timeout and then a stall, is
        # replaced once: a second replacement would start a second worker.
        # A stall's verdict still counts, though. Nothing the worker holds
        # can finish, so it is asked to give up the rest for the edge to
        # answer, and killed in a moment rather than after the drain
        # grace, since that ask may queue behind a stacks request the
        # first condemnation left blocked on the interpreter.

        if state.replaced or state.index in self.replacing:
            replacement = self.replacing.get(state.index)

            if (
                abandon is not None
                and abandon.get("which") == "all"
                and replacement is not None
                and replacement.old_pid == state.pid
                and not replacement.stalled
            ):
                replacement.stalled = True
                log(
                    f"worker {state.index} (pid {state.pid}) {reason} while being "
                    "replaced; nothing it holds can finish"
                )
                self.abandon_requests(state, abandon)
                now = time.monotonic()
                replacement.terminate_at = now
                replacement.terminated = True
                replacement.kill_at = now + STALLED_KILL_SECONDS
                self.signal(state.pid, signal.SIGTERM)

            return

        if self.control_listener is None:
            if not getattr(self, "_warned_no_control", False):
                self._warned_no_control = True
                log("no control socket, so workers cannot be replaced; the policy is ignored")

            return

        index = state.index

        # A replacement that keeps failing to start is not tried again;
        # the worker it was to replace keeps serving.

        if self.start_failures.get(index, 0) >= MAX_START_FAILURES:
            return

        state.replaced = True
        self.generation[index] = self.generation.get(index, 0) + 1
        replacement = Replacement(index, state.pid, reason, kind, cause)
        self.replacing[index] = replacement
        hard = hard or kind == "worker.timeout_killed"
        log(f"worker {index} (pid {state.pid}) {reason}; replacing it")

        if "limit" in reason:
            self.note_memory_recycle(index)
            self.capture_objects(state, replacement)

        if hard:
            self.routed[index] = None
            self.announce()

            # The requests are given up before the stacks are asked for:
            # the worker's control thread takes requests in order, and
            # reading stacks from inside waits on the interpreter, which
            # the stuck code may hold for good.

            if abandon is not None:
                self.abandon_requests(state, abandon)

            if kind == "worker.timeout_killed":
                self.capture_stacks(state, replacement)

            grace = self.group_of(index).hard_drain_grace
            replacement.terminate_at = time.monotonic() + (
                self.hard_drain_grace if grace is None else grace
            )

        replacement.new_pid = self.spawn(index, replacing=True)

    def abandon_requests(self, state: WorkerState, args: dict[str, str]) -> None:
        """Have a condemned worker give up requests that cannot finish.

        The worker answers without its interpreter, which the stuck code
        may hold, and ends each request with the reason given, which the
        edge turns into the client's error at once (section 7.6).
        """

        def answered(pending: Pending) -> None:
            answer = pending.answers.get(state.pid) or {}
            ids = answer.get("result", {}).get("abandoned", [])

            if ids:
                log(
                    f"worker {state.index} (pid {state.pid}) gave up "
                    f"{len(ids)} request{'s' if len(ids) != 1 else ''} for the edge to "
                    f"answer: {', '.join(ids)}"
                )

        self.ask(state, "abandon", args, 1.0, answered)

    def ask(
        self,
        state: WorkerState,
        op: str,
        args: dict[str, Any],
        timeout: float,
        then: Callable[[Pending], None],
    ) -> None:
        """Ask one worker something for the host itself; `then` gets the answer."""

        self.ask_workers({state.pid: state.index}, op, args, timeout, then)

    def ask_workers(
        self,
        targets: dict[int, int],
        op: str,
        args: dict[str, Any],
        timeout: float,
        then: Callable[[Pending], None],
    ) -> None:
        """Ask workers something for the host itself; `then` gets the answers."""

        self.own_requests -= 1
        pending = Pending(self.own_requests, op, dict(targets), then=then)
        pending.deadline = time.monotonic() + timeout
        self.pending[pending.id] = pending
        line = json.dumps({"id": pending.id, "op": op, "args": args}).encode() + b"\n"

        for pid in targets:
            control = self.control_sockets.get(pid)

            try:
                if control is None:
                    raise OSError("no control connection")

                control.sendall(line)
            except OSError as error:
                pending.answers[pid] = {"ok": False, "error": str(error)}

        self.finish_if_answered(pending)

    def read_outside(self, pid: int) -> list[tuple[int, list[dict[str, Any]]]] | None:
        """Read a worker's threads from outside, or return None.

        The first time reading is found impossible is logged.
        """

        known = self.outside.unreadable

        try:
            return self.outside.stacks(pid)
        except _outside.Unreadable as error:
            self.outside_errors[pid] = str(error)

            if known is None and self.outside.unreadable is not None:
                # Expected on macOS, where only a signed and entitled
                # process may read another's memory, so it is said as
                # information rather than left to read as an error.

                log(
                    "stacks and profiles come from inside the workers, as reading them from "
                    f"outside is not available here ({error})"
                    + ("; that is usual on macOS" if sys.platform == "darwin" else "")
                )

            return None

    def stacks_from_outside(self, request_id: int, targets: dict[int, int]) -> bool:
        """Answer a stacks request from outside, or return False.

        Each worker's frames are read now, and its bookkeeping, which the
        runtime gives without the interpreter, names the threads and the
        requests. False means reading from outside is not possible here.
        """

        read = {pid: self.read_outside(pid) for pid in targets}

        if self.outside.unreadable is not None:
            return False

        def answered(pending: Pending) -> None:
            workers = []

            for pid, index in targets.items():
                threads = read.get(pid)
                answer = pending.answers.get(pid, {})
                bookkeeping = answer.get("result", {}) if answer.get("ok") else {}

                if threads is None:
                    error = self.outside_errors.pop(pid, "")
                    workers.append(
                        {
                            "index": index,
                            "pid": pid,
                            "ok": False,
                            "error": f"could not be read from outside: {error}",
                        }
                    )
                    continue

                workers.append(
                    {
                        "index": index,
                        "pid": pid,
                        "ok": True,
                        "result": _outside.stacks_result(threads, bookkeeping),
                    }
                )

            self.send_to_supervisor(
                {
                    "type": "response",
                    "id": request_id,
                    "workers": sorted(workers, key=lambda worker: worker["index"]),
                }
            )

        self.ask_workers(targets, "bookkeeping", {}, 1.0, answered)
        return True

    def profile_from_outside(
        self, request_id: int, targets: dict[int, int], args: dict[str, Any]
    ) -> bool:
        """Sample the workers from outside, or return False.

        The sampling runs on a thread of the host's, which costs the
        workers nothing. False means reading from outside is not possible
        here.
        """

        if self.read_outside(next(iter(targets))) is None and self.outside.unreadable is not None:
            return False

        duration = float(args.get("duration_s", 10.0))
        interval = float(args.get("interval_ms", 10.0)) / 1000
        pending = Pending(request_id, "profile", dict(targets))
        pending.deadline = time.monotonic() + duration + 10
        self.pending[request_id] = pending

        def sample(names: dict[int, dict[str, str]]) -> None:
            results = self.outside.profile(list(targets), duration, interval, names)

            for pid, answer in results.items():
                pending.answers[pid] = {"index": targets[pid], "pid": pid, **answer}

            if self.wake_write is not None:
                try:
                    os.write(self.wake_write, b"\0")
                except OSError:
                    pass

        def named(bookkeeping: Pending) -> None:
            names = {
                pid: answer.get("result", {}).get("names", {})
                for pid, answer in bookkeeping.answers.items()
                if answer.get("ok")
            }
            threading.Thread(target=sample, args=(names,), name="muru-profile", daemon=True).start()

        self.ask_workers(targets, "bookkeeping", {}, 1.0, named)
        return True

    def capture_stacks(self, state: WorkerState, replacement: Replacement) -> None:
        """Take a worker's stacks before it is killed for the hard timeout.

        Section 11.7: the stacks go to the log and where the oldest request
        was goes to the event. They are read from outside where that is
        possible; otherwise the worker is asked, and one that cannot answer
        within a second gets SIGQUIT, and faulthandler writes its stacks to
        the log instead.
        """

        outside = self.read_outside(state.pid)

        if outside is not None:

            def read(pending: Pending) -> None:
                answer = pending.answers.get(state.pid, {})
                bookkeeping = answer.get("result", {}) if answer.get("ok") else {}
                result = _outside.stacks_result(outside, bookkeeping)
                self.record_capture(state, replacement, result["threads"])

            self.ask(state, "bookkeeping", {}, 1.0, read)
            return

        def answered(pending: Pending) -> None:
            answer = pending.answers.get(state.pid, {})

            if not answer.get("ok"):
                replacement.capture = "its stacks were written to the log by faulthandler"
                return

            self.record_capture(state, replacement, answer.get("result", {}).get("threads", []))

        self.ask(state, "stacks", {}, 1.0, answered)

    def record_capture(
        self, state: WorkerState, replacement: Replacement, threads: list[dict[str, Any]]
    ) -> None:
        """Log a worker's stacks and note where its oldest request was."""

        for thread in threads:
            frames = "".join(
                f"\n    {frame['file']}:{frame['line']} in {frame['function']}"
                for frame in thread.get("frames", [])
            )
            log(f"worker {state.index} (pid {state.pid}) {thread.get('name')}:{frames}")

        stuck = max(
            (thread for thread in threads if thread.get("request")),
            key=lambda thread: thread["request"].get("elapsed_ms", 0),
            default=None,
        )

        if stuck is not None and stuck.get("frames"):
            innermost = stuck["frames"][-3:][::-1]
            replacement.capture = "at " + ", called from ".join(
                f"{os.path.basename(frame['file'])} line {frame['line']} in {frame['function']}"
                for frame in innermost
            )

    def capture_objects(self, state: WorkerState, replacement: Replacement) -> None:
        """Count a worker's commonest objects before it is recycled for memory.

        Section 11.7: the counts go to the event, so the growth is attributed.
        """

        def answered(pending: Pending) -> None:
            answer = pending.answers.get(state.pid, {})

            if answer.get("ok"):
                objects = answer.get("result", {}).get("objects", [])
                replacement.capture = "most objects: " + ", ".join(
                    f"{name} {count}" for name, count in objects
                )

        self.ask(state, "memory", {"top": 5}, 2.0, answered)

    def advance(self, replacement: Replacement, now: float) -> None:
        """Move a replacement along.

        Once the edge routes to the new worker (or the wait for that word
        is up), the old one is told to stop, and killed if it does not.
        """

        if replacement.announced is not None and replacement.terminate_at is None:
            acknowledged = self.acked >= replacement.announced
            waited = now >= (replacement.announced_at or now) + ACK_TIMEOUT_SECONDS

            if acknowledged or waited:
                replacement.terminate_at = now + ROUTE_SETTLE_SECONDS

        if (
            replacement.terminate_at is not None
            and not replacement.terminated
            and now >= replacement.terminate_at
        ):
            replacement.terminated = True
            replacement.kill_at = now + self.stop_budget()
            self.signal(replacement.old_pid, signal.SIGTERM)

        if (
            replacement.kill_at is not None
            and not replacement.killed
            and now >= replacement.kill_at
        ):
            replacement.killed = True
            log(f"worker {replacement.index} (pid {replacement.old_pid}) did not stop, killing")
            self.signal(replacement.old_pid, signal.SIGKILL)

    def signal(self, pid: int, signum: int) -> None:
        """Send a signal to a worker that may already be gone."""

        try:
            os.kill(pid, signum)
        except ProcessLookupError:
            pass

    def reopen_if_requested(self) -> None:
        """Carry out what SIGUSR1 and SIGUSR2 asked for, outside the handlers."""

        if self.reopen_requested:
            self.reopen_requested = False
            self.reopen_log()

        if self.workers_reopen_requested:
            self.workers_reopen_requested = False

            for pid in list(self.workers):
                try:
                    os.kill(pid, signal.SIGUSR1)
                except ProcessLookupError:
                    pass

    def reopen_log(self) -> None:
        """Open the log file again by name on standard output and error.

        The workers' pipes are relayed through this process's standard
        error, so reopening it here is enough for their lines too.
        """

        if self.log_file is None:
            return

        try:
            fd = os.open(self.log_file, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
        except OSError as error:
            log(f"could not reopen {self.log_file}: {error}")
            return

        sys.stdout.flush()
        sys.stderr.flush()
        os.dup2(fd, 1)
        os.dup2(fd, 2)
        os.close(fd)
        log(f"reopened {self.log_file}")

    def read_output(self, fd: int, until_closed: bool = False) -> None:
        """Relay what a worker has written; close the pipe once it is at end of file."""

        output = self.outputs[fd]

        while True:
            try:
                data = os.read(fd, READ_SIZE)
            except BlockingIOError:
                if until_closed:
                    time.sleep(0.01)
                    continue

                return
            except OSError:
                data = b""

            if not data:
                output.flush()
                self.selector.unregister(fd)
                os.close(fd)
                del self.outputs[fd]
                return

            output.relay(data)

    def reap(self) -> None:
        """Collect exited workers and schedule their replacement.

        Section 6.10: a process a dead worker left behind, the host's
        child now, is collected here too when it exits.
        """

        while True:
            try:
                pid, status = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                return

            if pid == 0:
                return
            index = self.workers.pop(pid, None)

            if index is None:
                self.orphans.pop(pid, None)
                continue

            spawned_at = self.spawned_at.pop(pid, 0.0)
            self.states.pop(pid, None)
            self.adopt_orphans(index, pid)
            if self.stopping:
                continue

            # The old worker of a replacement is done with: its event
            # says why it went, and nothing restarts it.

            replacement = self.replacing.get(index)

            if replacement is not None and replacement.old_pid == pid and replacement.retire:
                del self.replacing[index]
                self.retired.add(index)
                self.emit(
                    "worker.exited",
                    "info",
                    f"worker {index} (pid {pid}) stopped: scaled in",
                    group=self.group_of(index).name,
                    reason="scaled_in",
                )
                continue

            if replacement is not None and replacement.old_pid == pid:
                del self.replacing[index]
                level = "warning" if replacement.kind == "worker.timeout_killed" else "info"
                capture = f"; {replacement.capture}" if replacement.capture else ""
                self.emit(
                    replacement.kind,
                    level,
                    f"worker {index} (pid {pid}) replaced by pid {replacement.new_pid}: "
                    f"{replacement.reason}{capture}",
                    group=self.group_of(index).name,
                    reason=replacement.cause,
                )
                continue

            # A replacement that died before it took over leaves the old
            # worker serving; it is tried again as a restart would be.

            if replacement is not None and replacement.new_pid == pid:
                del self.replacing[index]
                old = self.states.get(replacement.old_pid)

                if replacement.announced is None and old is not None:
                    old.replaced = False
                    self.start_failures[index] = self.start_failures.get(index, 0) + 1
                    self.emit(
                        "worker.exited",
                        "error",
                        f"worker {index} replacement (pid {pid}) exited with "
                        f"{describe(status)} before it was ready; pid {old.pid} keeps serving",
                        group=self.group_of(index).name,
                        reason="start_failed",
                    )
                    continue

            # A worker that dies at once, again and again, is a release that
            # cannot start (a failing factory or hook): give up rather than
            # fork it forever.

            if status != 0 and time.monotonic() - spawned_at < STARTUP_WINDOW_SECONDS:
                self.start_failures[index] = self.start_failures.get(index, 0) + 1

                if self.start_failures[index] >= MAX_START_FAILURES:
                    group = self.group_of(index)

                    # ADR 0033: the release is ready without an optional
                    # group, so its worker is given up and the index left
                    # for a scale-out to try again, while the others serve.

                    if group.optional:
                        self.start_failures[index] = 0
                        self.starting.discard(index)
                        self.routed[index] = None
                        self.retired.add(index)
                        self.emit(
                            "worker.exited",
                            "error",
                            f"worker {index} (pid {pid}) of the optional group {group.name} "
                            f"failed {MAX_START_FAILURES} times at start, last with "
                            f"{describe(status)}; it is given up",
                            group=group.name,
                            reason="start_failed",
                        )
                        self.announce()
                        continue

                    log(
                        f"worker {index} failed {MAX_START_FAILURES} times at start, "
                        f"last with {describe(status)}; stopping"
                    )
                    self.failed = True
                    self.stopping = True
                    continue
            else:
                self.start_failures[index] = 0

            self.emit(
                "worker.exited",
                "warning",
                f"worker {index} (pid {pid}) exited with {describe(status)}, restarting",
                group=self.group_of(index).name,
                reason="crashed",
            )
            self.restart_after[index] = time.monotonic() + RESTART_DELAY_SECONDS

    def restart_due(self) -> None:
        """Start replacements whose delay has passed."""

        now = time.monotonic()

        for index, when in list(self.restart_after.items()):
            if when <= now:
                del self.restart_after[index]
                self.spawn(index, replacing=index in self.starting)

    def adopt_orphans(self, index: int, pid: int) -> None:
        """Take charge of what a worker that has just gone left running.

        Section 6.10: with the host a subreaper, the worker's children are
        the host's children now. Each gets the shutdown timeout to finish
        on its own, is then told to stop, and is killed after as long again.
        """

        if not self.subreaper:
            return

        left = sorted(children_of(os.getpid()) - set(self.workers) - set(self.orphans))

        if not left:
            return

        now = time.monotonic()

        for child in left:
            self.orphans[child] = Orphan(child, index, now + self.shutdown_timeout)

        pids = ", ".join(str(child) for child in left)
        log(
            f"worker {index} (pid {pid}) left pid{'s' if len(left) != 1 else ''} {pids} running; "
            f"stopping {'them' if len(left) != 1 else 'it'} in {self.shutdown_timeout:g} s "
            f"unless {'they' if len(left) != 1 else 'it'} exit{'' if len(left) != 1 else 's'} first"
        )

    def advance_orphans(self, now: float) -> None:
        """Stop the processes dead workers left behind once their time is up."""

        for orphan in list(self.orphans.values()):
            if not orphan.terminated and now >= orphan.terminate_at:
                orphan.terminated = True
                orphan.kill_at = now + self.shutdown_timeout
                self.signal(orphan.pid, signal.SIGTERM)

            if orphan.kill_at is not None and not orphan.killed and now >= orphan.kill_at:
                orphan.killed = True
                log(f"pid {orphan.pid}, left by worker {orphan.left_by}, did not stop, killing")
                self.signal(orphan.pid, signal.SIGKILL)

    def stop_orphans(self) -> None:
        """Stop and collect whatever the workers left running, as the host stops.

        Section 2.4: nothing of a release outlives its host. The leftovers
        get the shutdown timeout to stop before they are killed, and the
        pipes they inherited close with them, so relaying the workers'
        last output does not wait on them.
        """

        left = children_of(os.getpid()) - set(self.workers)

        if not left:
            self.orphans.clear()
            return

        for pid in left:
            self.signal(pid, signal.SIGTERM)

        # Reap whatever stops within the grace period.

        deadline = time.monotonic() + self.shutdown_timeout

        while left and time.monotonic() < deadline:
            try:
                pid, _status = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                left.clear()
                break

            if pid == 0:
                time.sleep(0.05)
                continue

            left.discard(pid)

        # Whatever is left is killed and reaped.

        for pid in sorted(left):
            log(f"pid {pid}, left by a worker, did not stop, killing")

            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass

        self.orphans.clear()

    def stop_budget(self) -> float:
        """Return how long a worker told to stop may take before it is killed.

        That is the WebSockets' grace, then the shutdown timeout.
        """

        return self.websocket_drain_grace + self.shutdown_timeout

    def terminate_all(self) -> None:
        """Stop every worker: SIGTERM, its stop budget, then SIGKILL."""

        for pid in self.workers:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass

        # Reap whatever exits within the grace period.

        deadline = time.monotonic() + self.stop_budget()

        while self.workers and time.monotonic() < deadline:
            try:
                pid, _status = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                self.workers.clear()
                break

            if pid == 0:
                time.sleep(0.05)
                continue

            self.workers.pop(pid, None)

        # Whatever is left is killed and reaped.

        for pid in list(self.workers):
            log(f"worker pid {pid} did not stop, killing")

            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass

        self.workers.clear()
        self.stop_orphans()

        if self.tcp_base_port is None:
            paths = {self.socket_path(index) for index in range(self.worker_count)}
            paths.update(state.path for state in self.states.values())

            for path in paths:
                try:
                    os.unlink(path)
                except OSError:
                    pass

        self.drop_supervisor()

        if self.control_listener is not None:
            self.control_listener.close()

            try:
                os.unlink(self.control_path or "")
            except OSError:
                pass


def format_size(size: int) -> str:
    """Write a size in the largest unit that keeps a digit, as 1.5GB or 800MB."""

    value = float(size)

    for unit in ("B", "KB", "MB", "GB"):
        if value < 1024 or unit == "GB":
            return f"{value:.0f}{unit}" if unit == "B" or value >= 10 else f"{value:.1f}{unit}"

        value /= 1024

    return f"{size}B"


def malloc_trim() -> bool:
    """Return heap memory to the system on glibc; false where there is no malloc_trim."""

    try:
        import ctypes

        libc = ctypes.CDLL("libc.so.6")
        return bool(libc.malloc_trim(0))
    except (OSError, AttributeError):
        return False


# Section 6.10: the prctl(2) option that makes orphaned descendants
# reparent to this process rather than to init.

PR_SET_CHILD_SUBREAPER = 36


def become_subreaper() -> bool:
    """Make orphaned descendants reparent to this process, on Linux.

    Section 6.10: a worker's children that outlive it come to the host
    rather than to init, so the host can stop and collect them. Returns
    whether it took, which it never does outside Linux, where the C
    library has no prctl.
    """

    try:
        import ctypes

        libc = ctypes.CDLL(None, use_errno=True)
        return int(libc.prctl(PR_SET_CHILD_SUBREAPER, ctypes.c_ulong(1), 0, 0, 0)) == 0
    except (OSError, AttributeError):
        return False


def children_of(pid: int) -> set[int]:
    """Return the pids whose parent is `pid` now, read from /proc.

    Empty where there is no /proc, which is everywhere but Linux.
    """

    parent = str(pid).encode()
    children: set[int] = set()

    try:
        entries = os.listdir("/proc")
    except OSError:
        return children

    for entry in entries:
        if not entry.isdigit():
            continue

        try:
            with open(f"/proc/{entry}/stat", "rb") as handle:
                stat = handle.read()
        except OSError:
            continue

        # The command name may hold spaces and parentheses, so the fields
        # are read after its closing parenthesis: the state, then the
        # parent's pid.

        fields = stat[stat.rfind(b")") + 1 :].split()

        if len(fields) > 1 and fields[1] == parent:
            children.add(int(entry))

    return children


def describe(status: int) -> str:
    """Describe a waitpid status in words."""

    if os.WIFEXITED(status):
        return f"status {os.WEXITSTATUS(status)}"
    if os.WIFSIGNALED(status):
        return f"signal {signal.Signals(os.WTERMSIG(status)).name}"
    return f"status {status}"


def elapsed_ms(since: float) -> str:
    """Format the time since a monotonic timestamp in whole milliseconds."""

    return f"{(time.monotonic() - since) * 1000:.0f}"


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    """Parse the host's command line."""

    parser = argparse.ArgumentParser(
        prog="python -m muru.host",
        description="The host process muructl starts for a release: it imports the application "
        "once and forks its workers.",
    )
    parser.add_argument("--app", required=True, help="application as module:callable")
    parser.add_argument(
        "--name",
        default=None,
        help="the application's name for muru.context(); the entrypoint's module otherwise",
    )
    parser.add_argument(
        "--release", default=None, help="the release's id for muru.context(); dev otherwise"
    )
    parser.add_argument("--socket-dir", required=True, help="directory for the worker sockets")
    parser.add_argument("--processes", type=int, default=1, help="worker processes to fork")
    parser.add_argument(
        "--interface",
        choices=INTERFACES,
        default="auto",
        help="the interface the entrypoint speaks; auto detects it from the callable",
    )
    parser.add_argument(
        "--reflection",
        action="store_true",
        help="serve gRPC server reflection for the registered services",
    )
    parser.add_argument(
        "--factory",
        action="store_true",
        help="the entrypoint is a factory returning the application",
    )
    parser.add_argument(
        "--factory-phase",
        choices=FACTORY_PHASES,
        default="worker",
        help="call the factory in each worker after post_fork, or once in the host",
    )
    parser.add_argument("--threads", type=int, default=4, help="application threads per worker")
    parser.add_argument(
        "--sync-threads",
        type=int,
        default=8,
        help="threads for synchronous code called from an ASGI application",
    )
    parser.add_argument(
        "--concurrency",
        type=int,
        default=1000,
        help="requests an ASGI worker's event loop takes at once",
    )
    parser.add_argument(
        "--preload",
        choices=PRELOAD_MODES,
        default="auto",
        help="import in the host before forking: auto (with the import check), true or false",
    )
    parser.add_argument("--hooks", default=None, help="module providing the lifecycle hooks")
    parser.add_argument(
        "--single",
        action="store_true",
        help="serve in this process with one thread and no fork, for debuggers",
    )
    parser.add_argument(
        "--tcp-base-port",
        type=int,
        default=None,
        help="listen on loopback TCP ports from this one instead of Unix sockets (experiments)",
    )
    parser.add_argument(
        "--preload-gc-collect",
        action="store_true",
        help="run one full collection after preload",
    )
    parser.add_argument(
        "--detach",
        action="store_true",
        help="keep running when the parent goes away, for a daemon that re-adopts hosts",
    )
    parser.add_argument(
        "--log-file",
        default=None,
        metavar="PATH",
        help="the file standard output and error go to, reopened by name on SIGUSR1",
    )
    parser.add_argument(
        "--group",
        action="append",
        default=[],
        type=Group.parse,
        metavar="NAME=PROCESSES,THREADS,SYNC_THREADS,CONCURRENCY",
        help="a named worker group beside the default one",
    )
    parser.add_argument(
        "--optional-group",
        action="append",
        default=[],
        metavar="NAME",
        help="a group the release is ready without; its workers join as each listens",
    )
    parser.add_argument(
        "--group-mounts",
        action="append",
        default=[],
        type=parse_group_mounts,
        metavar="NAME=MOUNT,...",
        help="the named mounts a group's workers serve beside the default; every mount without",
    )
    parser.add_argument(
        "--group-timeouts",
        action="append",
        default=[],
        type=parse_group_timeouts,
        metavar="NAME=SOFT,HARD,HARD_DRAIN_GRACE,LOOP_STALL,GIL_STALL",
        help="a named group's timeouts in seconds, or off; the release's without",
    )
    parser.add_argument(
        "--group-memory",
        action="append",
        default=[],
        type=parse_group_memory,
        metavar="NAME=MEASURE,SOFT_LIMIT,SOFT_FOR,RECLAIM_FIRST,HARD_LIMIT,CHECK_INTERVAL",
        help="a named group's memory policy, limits in bytes or off; the release's without",
    )
    parser.add_argument(
        "--mount",
        action="append",
        default=[],
        metavar="NAME=PATH=ENTRYPOINT[=INTERFACE[=RESERVED,MAX]]",
        help="a named mount served by another entrypoint at a path; for an ASGI mount "
        "the last part is CONCURRENCY,CONNECTIONS",
    )
    parser.add_argument(
        "--log-level",
        choices=LOG_LEVELS,
        default=None,
        help="the root logger's level; Python's WARNING when not given",
    )
    parser.add_argument(
        "--log-format",
        default=None,
        metavar="FORMAT",
        help="standard, json, or a logging format string for loggers without handlers",
    )
    parser.add_argument(
        "--capture-warnings",
        action="store_true",
        help="route warnings.warn through the py.warnings logger",
    )
    parser.add_argument(
        "--logger",
        action="append",
        default=[],
        type=parse_logger_level,
        metavar="NAME=LEVEL",
        help="a named logger's level",
    )
    parser.add_argument(
        "--control",
        default=None,
        metavar="PATH",
        help="the Unix socket the supervisor connects to for worker sockets and events",
    )
    parser.add_argument(
        "--max-requests",
        type=int,
        default=None,
        metavar="N",
        help="replace a worker after this many requests, with jitter",
    )
    parser.add_argument(
        "--max-lifetime",
        type=float,
        default=None,
        metavar="SECONDS",
        help="replace a worker after this long, with jitter",
    )
    parser.add_argument(
        "--soft-timeout",
        type=float,
        default=None,
        metavar="SECONDS",
        help="interrupt a request that has run this long with muru.RequestTimeout",
    )
    parser.add_argument(
        "--hard-timeout",
        type=float,
        default=None,
        metavar="SECONDS",
        help="replace a worker whose request has run this long",
    )
    parser.add_argument(
        "--hard-drain-grace",
        type=float,
        default=10.0,
        metavar="SECONDS",
        help="how long a replaced worker gets to finish its other requests",
    )
    parser.add_argument(
        "--loop-stall-timeout",
        type=float,
        default=10.0,
        metavar="SECONDS",
        help="replace an ASGI worker whose event loop has not run for this long",
    )
    parser.add_argument(
        "--gil-stall-timeout",
        type=float,
        default=30.0,
        metavar="SECONDS",
        help="replace a worker in which no thread has run Python for this long, or that "
        "has not reported for this long",
    )
    parser.add_argument(
        "--shutdown-timeout",
        type=float,
        default=5.0,
        metavar="SECONDS",
        help="how long a worker told to stop gets before it is killed, after its WebSockets close",
    )
    parser.add_argument(
        "--websocket-drain-grace",
        type=float,
        default=0.0,
        metavar="SECONDS",
        help="how long a worker told to stop keeps its open WebSockets before closing them",
    )
    parser.add_argument(
        "--memory-measure",
        choices=("uss", "pss", "rss"),
        default="uss",
        help="what the memory limits measure",
    )
    parser.add_argument(
        "--memory-soft-limit",
        type=int,
        default=None,
        metavar="BYTES",
        help="over this for --memory-soft-for, a worker reclaims and then is replaced",
    )
    parser.add_argument(
        "--memory-soft-for",
        type=float,
        default=120.0,
        metavar="SECONDS",
        help="how long a worker may stay over the soft limit",
    )
    parser.add_argument(
        "--memory-reclaim-first",
        choices=("true", "false"),
        default="true",
        help="collect garbage and trim the heap before replacing over the soft limit",
    )
    parser.add_argument(
        "--memory-hard-limit",
        type=int,
        default=None,
        metavar="BYTES",
        help="over this a worker leaves routing at once and is replaced",
    )
    parser.add_argument(
        "--memory-check-interval",
        type=float,
        default=5.0,
        metavar="SECONDS",
        help="how often a worker measures its memory",
    )
    return parser.parse_args(argv)


def parse_logger_level(text: str) -> tuple[str, str]:
    """Parse NAME=LEVEL for --logger."""

    name, separator, level = text.partition("=")

    if not separator or not name or level not in LOG_LEVELS:
        raise argparse.ArgumentTypeError(
            f"{text!r} is not NAME=LEVEL with a level of {', '.join(LOG_LEVELS)}"
        )

    return name, level


def without_preload(argv: list[str]) -> list[str]:
    """Return the arguments with any --preload option removed."""

    kept: list[str] = []
    skip = False

    for argument in argv:
        if skip:
            skip = False
            continue

        if argument == "--preload":
            skip = True
            continue

        if argument.startswith("--preload="):
            continue
        kept.append(argument)

    return kept


def main(argv: list[str] | None = None) -> int:
    """Run the host: preload, fork, supervise."""

    arguments = list(sys.argv[1:] if argv is None else argv)
    args = parse_args(arguments)

    if args.processes < 1:
        print("--processes must be at least 1", file=sys.stderr)
        return 2

    context = Context(
        app=args.name or app_name(args.app),
        release=args.release or "dev",
        process_model=process_model(args.threads),
    )
    set_context(context)
    release = Release(
        args.app,
        args.hooks,
        context,
        threads=args.threads,
        interface=args.interface,
        factory=args.factory,
        factory_phase=args.factory_phase,
    )

    try:
        mounts = [Mount.parse(text, context, args.threads) for text in args.mount]
    except argparse.ArgumentTypeError as error:
        print(f"--mount: {error}", file=sys.stderr)
        return 2

    # Section 11.4: the logging defaults are in place before anything of
    # the application is imported, and the workers inherit them.

    muru_logging._install(args.log_level, args.log_format, args.capture_warnings, dict(args.logger))

    # Section 6.2 step 1: no collections during preload.

    gc.disable()

    if args.preload != "false":
        started = time.monotonic()

        with preload.ImportRecorder() as recorder:
            try:
                hooks = release.load(recorder)
                for mount in mounts:
                    mount.release.load(recorder)
            except Exception as error:
                log(f"cannot load {args.app}: {error.__class__.__name__}: {error}")
                return 1

            if not hooks.call_logged("on_load", context):
                return 1

        log(f"preloaded {args.app} in {elapsed_ms(started)} ms")

        # A preloaded application has settled its interface by now; a
        # per-worker factory settles it in each worker.

        if release.resolved is not None:
            set_context(release.context)
            log(f"{args.app} is served as {interface_name(release.resolved)}")

        for mount in mounts:
            if mount.release.resolved is not None:
                log(
                    f"mount {mount.name}: {mount.release.spec} is served as "
                    f"{interface_name(mount.release.resolved)}"
                )

        findings = recorder.findings()

        if findings:
            for finding in findings:
                log(f"preload: {finding.describe()}; {finding.advice}")

            if args.preload == "true":
                log("preload is true, so the import check failing stops the release")
                return 1

            log(
                "preload: falling back to per-worker imports, which costs slower worker "
                "replacement and no shared memory; set preload to false to silence this"
            )
            sys.stderr.flush()
            os.execv(
                sys.executable,
                [
                    sys.executable,
                    "-m",
                    "muru.host",
                    *without_preload(arguments),
                    "--preload",
                    "false",
                ],
            )

        if args.preload_gc_collect:
            gc.collect()

    # A socket past the platform's path limit never binds, so the longest
    # a default worker's socket can be is checked once here; a named
    # group's sockets carry its name as well.

    longest = os.path.join(args.socket_dir, "worker-99-g99.sock")

    if len(longest.encode()) >= SOCKET_PATH_LIMIT:
        log(
            f"the worker socket path {longest} would be {len(longest.encode())} bytes, past "
            f"the {SOCKET_PATH_LIMIT}-byte limit of this platform; give the daemon or "
            "muructl run a shorter run directory"
        )
        return 1

    os.makedirs(args.socket_dir, exist_ok=True)
    host = Host(
        release,
        args.socket_dir,
        args.processes,
        args.threads,
        args.tcp_base_port,
        sync_threads=args.sync_threads,
        concurrency=args.concurrency,
        detach=args.detach,
        groups=[
            replace(
                group,
                **dict(args.group_timeouts).get(group.name, {}),
                memory=dict(args.group_memory).get(group.name),
                optional=group.name in args.optional_group,
                mounts=dict(args.group_mounts).get(group.name, ()),
            )
            for group in args.group
        ],
        mounts=mounts,
        log_file=args.log_file,
        control=args.control,
        max_requests=args.max_requests,
        max_lifetime=args.max_lifetime,
        soft_timeout=args.soft_timeout,
        hard_timeout=args.hard_timeout,
        hard_drain_grace=args.hard_drain_grace,
        shutdown_timeout=args.shutdown_timeout,
        websocket_drain_grace=args.websocket_drain_grace,
        loop_stall_timeout=args.loop_stall_timeout,
        gil_stall_timeout=args.gil_stall_timeout,
        memory=MemoryPolicy(
            measure=args.memory_measure,
            soft_limit=args.memory_soft_limit,
            soft_for=args.memory_soft_for,
            reclaim_first=args.memory_reclaim_first == "true",
            hard_limit=args.memory_hard_limit,
            check_interval=args.memory_check_interval,
        ),
        reflection=args.reflection,
    )

    if args.single:
        # Section 13.1: no fork, so a debugger attached to this process sees
        # the application. The hooks run as they would in a worker.

        gc.enable()
        log("serving in this process without forking (--single)")
        return host.worker(0)

    return host.run()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
