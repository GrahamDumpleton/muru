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
"""

from __future__ import annotations

import argparse
import gc
import importlib
import inspect
import os
import selectors
import signal
import sys
import time
import traceback
from collections.abc import Callable
from dataclasses import dataclass, replace
from types import ModuleType
from typing import Any

from muru import _asgi, preload
from muru import logging as muru_logging
from muru._context import Context
from muru._context import _set as set_context
from muru.hooks import Hooks

WSGIApplication = Callable[..., Any]

SHUTDOWN_GRACE_SECONDS = 10.0
RESTART_DELAY_SECONDS = 0.5

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


INTERFACES = ("auto", "wsgi", "asgi")
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


def resolve_interface(spec: str, app: Any, interface: str) -> str:
    """Return the interface the application is served with: ``wsgi`` or ``asgi``.

    ``auto`` detects it from the callable. A declared interface is trusted,
    except that an application declared WSGI whose callable is a coroutine
    function cannot be served that way and is refused.
    """

    detected = detect_interface(app)
    if interface == "auto":
        return detected

    if interface == "wsgi" and detected == "asgi":
        raise RuntimeError(
            f"{spec} is declared WSGI but is a coroutine function; declare it as asgi or "
            "leave the interface to be detected"
        )

    return interface


def app_name(spec: str) -> str:
    """Name the application after the top-level module of its entrypoint."""

    module_name = spec.partition(":")[0]
    return module_name.split(".")[0] or "default"


def process_model(threads: int, interface: str = "wsgi") -> str:
    """Name the process model of section 6.4 for a release."""

    if interface == "asgi":
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
        """Keep the application and settle the interface it is served with."""

        self.resolved = resolve_interface(self.spec, app, self.interface)
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
    without hooks of its own, and shares the worker's threads and loop.
    """

    name: str
    path: str
    release: Release

    @classmethod
    def parse(cls, text: str, context: Context, threads: int) -> Mount:
        """Parse NAME=PATH=ENTRYPOINT[=INTERFACE]."""

        parts = text.split("=")
        if len(parts) not in (3, 4) or not parts[0] or not parts[1].startswith("/"):
            raise argparse.ArgumentTypeError(f"{text!r} is not NAME=PATH=ENTRYPOINT[=INTERFACE]")

        interface = parts[3] if len(parts) == 4 else "auto"
        if interface not in INTERFACES:
            raise argparse.ArgumentTypeError(f"{interface!r} is not an interface")

        release = Release(parts[2], None, context, threads=threads, interface=interface)
        return cls(parts[0], parts[1], release)


@dataclass(frozen=True)
class Group:
    """A worker group (section 7.1): its name and sizes.

    The default group, named "default", carries the release's own
    settings; named groups follow it in the order given.
    """

    name: str
    processes: int
    threads: int
    sync_threads: int
    concurrency: int

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
    ) -> None:
        self.release = release
        self.mounts = mounts or []
        self.socket_dir = socket_dir
        self.tcp_base_port = tcp_base_port
        self.detach = detach
        self.log_file = log_file
        self.threads = threads
        self.sync_threads = sync_threads
        self.concurrency = concurrency

        # Workers are numbered across the groups in the order they are
        # forked: the default group first, then each named group, each
        # worker knowing its group and its index within it.

        self.groups = [Group("default", workers, threads, sync_threads, concurrency)]
        self.groups.extend(groups or [])
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

    def socket_path(self, index: int) -> str:
        """Return the Unix socket path, or the TCP address, a worker listens on.

        A named group's sockets carry its name, so the daemon can tell
        them apart; the default group's are numbered plainly.
        """

        if self.tcp_base_port is not None:
            return f"127.0.0.1:{self.tcp_base_port + index}"

        group, within = self.placement[index]
        if group.name == "default":
            return os.path.join(self.socket_dir, f"worker-{within}.sock")

        return os.path.join(self.socket_dir, f"worker-{group.name}-{within}.sock")

    def spawn(self, index: int) -> None:
        """Fork one worker. The parent returns at once; the child never returns."""

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

        # Section 6.2: freeze before every fork so the collector in the
        # worker never touches the objects it inherited.

        gc.freeze()
        pid = os.fork()

        if pid == 0:
            status = 1

            try:
                os.dup2(out_write, 1)
                os.dup2(err_write, 2)
                for fd in (out_read, out_write, err_read, err_write, *self.outputs):
                    os.close(fd)
                os.close(self.lifeline_write)
                status = self.worker(index, lifeline=self.lifeline_read)
            except BaseException:
                traceback.print_exc()
            finally:
                os._exit(status)

        os.close(out_write)
        os.close(err_write)

        for fd, stream in ((out_read, "stdout"), (err_read, "stderr")):
            os.set_blocking(fd, False)
            self.outputs[fd] = WorkerOutput(index, stream, fd)
            self.selector.register(fd, selectors.EVENT_READ)

        self.workers[pid] = index
        self.spawned_at[pid] = time.monotonic()
        log(f"worker {index} started as pid {pid}")

    def worker(self, index: int, lifeline: int | None = None) -> int:
        """Run a worker: enable collection, run the hooks, then serve until the server stops.

        `lifeline` is the read end of the host's pipe, which the server
        watches so the worker stops when the host is gone.
        """

        global _in_worker
        _in_worker = True
        gc.enable()

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

        try:
            if release.preloaded:
                hooks = release.hooks or Hooks()
            else:
                # Per-worker imports: everything the host would have done.

                started = time.monotonic()
                hooks = release.load()
                for mount in self.mounts:
                    mount.release.load()
                log(f"worker {index} imported {release.spec} in {elapsed_ms(started)} ms")
                hooks.call("on_load", context)
                hooks.call("pre_fork", context)

            hooks.call("post_fork", context)

            # Section 6.8: a factory is called per worker, after post_fork,
            # and only then is the interface known for certain.

            known = release.resolved is not None
            app = release.realise()
            context = replace(release.context, worker_id=within, group=group.name)
            set_context(context)
            if not known and release.resolved is not None:
                log(f"worker {index} serves {release.spec} as {release.resolved.upper()}")

            # Section 6.7: each mount's application is realised like the
            # release's own; section 6.6: one event loop per worker when
            # any of them is ASGI, started after the fork, with lifespan
            # startup for each ASGI application before the worker is ready.

            realised = [
                (mount.name, mount.release.realise(), mount.release) for mount in self.mounts
            ]
            any_asgi = release.resolved == "asgi" or any(
                mounted.resolved == "asgi" for _, _, mounted in realised
            )

            if any_asgi:
                loop = _asgi.Loop(group.sync_threads)
                loop.start()

            if loop is not None and release.resolved == "asgi":
                lifespan = _asgi.Lifespan(app, loop, release.spec)
                lifespan.startup()

            lifespans: list[_asgi.Lifespan] = []
            served_mounts: list[tuple[str, Any, Any]] = []

            for name, mounted_app, mounted in realised:
                if loop is not None and mounted.resolved == "asgi":
                    mounted_lifespan = _asgi.Lifespan(mounted_app, loop, mounted.spec)
                    mounted_lifespan.startup()
                    lifespans.append(mounted_lifespan)
                    runner = _asgi.runner(mounted_app, loop, mounted_lifespan.state)
                    served_mounts.append((name, mounted_app, runner))
                else:
                    served_mounts.append((name, mounted_app, None))

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

        try:
            _native.serve(
                self.socket_path(index),
                app,
                group.threads if loop is None else 1,
                # Section 6.5: False only for a single-process development run.
                multiprocess=self.worker_count > 1,
                on_drain=lambda: hooks.call_logged("on_drain", context),
                on_exit=on_exit,
                on_reopen_logs=lambda: hooks.call_logged("on_reopen_logs", context),
                lifeline=lifeline,
                mounts=served_mounts,
                **asgi_arguments,
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

        def request_stop(signum: int, _frame: Any) -> None:
            if not self.stopping:
                log(f"received {signal.Signals(signum).name}, stopping")
            self.stopping = True

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

        for index in range(self.worker_count):
            self.spawn(index)

        while True:
            timeout = 0.1 if self.restart_after else None

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

            self.reap()
            self.reopen_if_requested()
            if self.stopping:
                break
            self.restart_due()

        self.terminate_all()
        for fd in list(self.outputs):
            self.read_output(fd, until_closed=True)
        signal.set_wakeup_fd(-1)
        return 1 if self.failed else 0

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
        """Collect exited workers and schedule their replacement."""

        while self.workers:
            try:
                pid, status = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                return

            if pid == 0:
                return
            index = self.workers.pop(pid, None)
            if index is None:
                continue
            spawned_at = self.spawned_at.pop(pid, 0.0)
            if self.stopping:
                continue

            # A worker that dies at once, again and again, is a release that
            # cannot start (a failing factory or hook): give up rather than
            # fork it forever.

            if status != 0 and time.monotonic() - spawned_at < STARTUP_WINDOW_SECONDS:
                self.start_failures[index] = self.start_failures.get(index, 0) + 1

                if self.start_failures[index] >= MAX_START_FAILURES:
                    log(
                        f"worker {index} failed {MAX_START_FAILURES} times at start, "
                        f"last with {describe(status)}; stopping"
                    )
                    self.failed = True
                    self.stopping = True
                    continue
            else:
                self.start_failures[index] = 0

            log(f"worker {index} (pid {pid}) exited with {describe(status)}, restarting")
            self.restart_after[index] = time.monotonic() + RESTART_DELAY_SECONDS

    def restart_due(self) -> None:
        """Start replacements whose delay has passed."""

        now = time.monotonic()

        for index, when in list(self.restart_after.items()):
            if when <= now:
                del self.restart_after[index]
                self.spawn(index)

    def terminate_all(self) -> None:
        """Stop every worker: SIGTERM, a grace period, then SIGKILL."""

        for pid in self.workers:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass

        # Reap whatever exits within the grace period.

        deadline = time.monotonic() + SHUTDOWN_GRACE_SECONDS

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

        if self.tcp_base_port is None:
            for index in range(self.worker_count):
                try:
                    os.unlink(self.socket_path(index))
                except OSError:
                    pass


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

    parser = argparse.ArgumentParser(prog="python -m muru.host", description=__doc__)
    parser.add_argument("--app", required=True, help="application as module:callable")
    parser.add_argument("--socket-dir", required=True, help="directory for the worker sockets")
    parser.add_argument("--processes", type=int, default=1, help="worker processes to fork")
    parser.add_argument(
        "--interface",
        choices=INTERFACES,
        default="auto",
        help="the interface the entrypoint speaks; auto detects it from the callable",
    )
    parser.add_argument(
        "--factory",
        action="store_true",
        help="the entrypoint is a factory returning the application (section 6.8)",
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
        help="serve in this process with one thread and no fork, for debuggers (section 13.1)",
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
        help="run one full collection after preload (section 6.2 step 2)",
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
        help="a named worker group beside the default one (section 7.1)",
    )
    parser.add_argument(
        "--mount",
        action="append",
        default=[],
        metavar="NAME=PATH=ENTRYPOINT[=INTERFACE]",
        help="a named mount served by another entrypoint at a path (section 6.7)",
    )
    parser.add_argument(
        "--log-level",
        choices=LOG_LEVELS,
        default=None,
        help="the root logger's level (section 11.4); Python's WARNING when not given",
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

    context = Context(app=app_name(args.app), process_model=process_model(args.threads))
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
            log(f"{args.app} is served as {release.resolved.upper()}")

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
        groups=args.group,
        mounts=mounts,
        log_file=args.log_file,
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
