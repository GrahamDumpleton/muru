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

One application, a fixed number of workers restarted when they die, and
shutdown on SIGTERM, SIGINT or when standard input closes, which is how the
parent `muructl` signals that it has gone away.
"""

from __future__ import annotations

import argparse
import gc
import importlib
import os
import selectors
import signal
import sys
import time
import traceback
from collections.abc import Callable
from dataclasses import dataclass
from types import ModuleType
from typing import Any

from muru import preload
from muru._context import Context
from muru._context import _set as set_context
from muru.hooks import Hooks

WSGIApplication = Callable[..., Any]

SHUTDOWN_GRACE_SECONDS = 10.0
RESTART_DELAY_SECONDS = 0.5
PRELOAD_MODES = ("auto", "true", "false")


def log(message: str) -> None:
    """Write one line to standard error, prefixed so it can be told from application output."""

    print(f"muru.host[{os.getpid()}]: {message}", file=sys.stderr, flush=True)


def resolve_app(
    spec: str, import_module: Callable[[str], ModuleType] = importlib.import_module
) -> WSGIApplication:
    """Import ``module:attribute`` and return the callable.

    The attribute defaults to ``application``.
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
    app: WSGIApplication = target
    return app


def app_name(spec: str) -> str:
    """Name the application after the top-level module of its entrypoint."""

    module_name = spec.partition(":")[0]
    return module_name.split(".")[0] or "default"


def process_model(threads: int) -> str:
    """Name the process model of section 6.4 for a WSGI release."""

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
    app: WSGIApplication | None = None
    hooks: Hooks | None = None

    @property
    def preloaded(self) -> bool:
        """Whether the application was imported in the host."""

        return self.app is not None

    def load(self, recorder: preload.ImportRecorder | None = None) -> Hooks:
        """Import the hooks module and the application, in that order."""

        import_module = recorder.import_module if recorder else importlib.import_module
        if self.hooks_module:
            import_module(self.hooks_module)
        self.hooks = Hooks.load(self.hooks_module)
        self.app = resolve_app(self.spec, import_module)
        return self.hooks


class Host:
    """Preload once, fork workers, and keep them running until told to stop."""

    def __init__(
        self,
        release: Release,
        socket_dir: str,
        workers: int,
        threads: int,
        tcp_base_port: int | None = None,
    ) -> None:
        self.release = release
        self.socket_dir = socket_dir
        self.tcp_base_port = tcp_base_port
        self.worker_count = workers
        self.threads = threads
        self.workers: dict[int, int] = {}
        self.stopping = False
        self.restart_after: dict[int, float] = {}
        self.forked_before = False

    def socket_path(self, index: int) -> str:
        """Return the Unix socket path, or the TCP address, a worker listens on."""

        if self.tcp_base_port is not None:
            return f"127.0.0.1:{self.tcp_base_port + index}"
        return os.path.join(self.socket_dir, f"worker-{index}.sock")

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
        # Section 6.2: freeze before every fork so the collector in the
        # worker never touches the objects it inherited.
        gc.freeze()
        pid = os.fork()
        if pid == 0:
            status = 1
            try:
                status = self.worker(index)
            except BaseException:
                traceback.print_exc()
            finally:
                os._exit(status)
        self.workers[pid] = index
        log(f"worker {index} started as pid {pid}")

    def worker(self, index: int) -> int:
        """Run a worker: enable collection, run the hooks, then serve until the server stops."""

        gc.enable()
        # The host's handlers and wake-up pipe belong to the host; the native
        # server installs its own handling of SIGTERM, SIGINT and SIGUSR1.
        signal.set_wakeup_fd(-1)
        for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGCHLD):
            signal.signal(signum, signal.SIG_DFL)
        release = self.release
        context = Context(
            app=release.context.app,
            release=release.context.release,
            group=release.context.group,
            worker_id=index,
            interface=release.context.interface,
            process_model=release.context.process_model,
        )
        set_context(context)

        try:
            if release.preloaded:
                hooks = release.hooks or Hooks()
            else:
                # Per-worker imports: everything the host would have done.
                started = time.monotonic()
                hooks = release.load()
                log(f"worker {index} imported {release.spec} in {elapsed_ms(started)} ms")
                hooks.call("on_load", context)
                hooks.call("pre_fork", context)
            hooks.call("post_fork", context)
            hooks.call("on_ready", context)
        except Exception:
            log(f"worker {index} failed in a hook")
            traceback.print_exc()
            return 1
        app = release.app
        if app is None:
            return 1

        from muru import _native

        try:
            _native.serve(
                self.socket_path(index),
                app,
                self.threads,
                on_drain=lambda: hooks.call_logged("on_drain", context),
                on_exit=lambda: hooks.call_logged("on_exit", context),
                on_reopen_logs=lambda: hooks.call_logged("on_reopen_logs", context),
            )
        except Exception as error:
            log(f"worker {index} failed: {error}")
            return 1
        return 0

    def run(self) -> int:
        """Supervise the workers until shutdown."""

        selector = selectors.DefaultSelector()
        wake_read, wake_write = os.pipe()
        os.set_blocking(wake_write, False)
        signal.set_wakeup_fd(wake_write, warn_on_full_buffer=False)
        selector.register(wake_read, selectors.EVENT_READ)

        def request_stop(signum: int, _frame: Any) -> None:
            if not self.stopping:
                log(f"received {signal.Signals(signum).name}, stopping")
            self.stopping = True

        signal.signal(signal.SIGTERM, request_stop)
        signal.signal(signal.SIGINT, request_stop)
        signal.signal(signal.SIGCHLD, lambda _signum, _frame: None)

        watch_stdin = False
        try:
            if not os.isatty(0):
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
            self.reap()
            if self.stopping:
                break
            self.restart_due()

        self.terminate_all()
        signal.set_wakeup_fd(-1)
        return 0

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
            if not self.stopping:
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
    parser.add_argument("--workers", type=int, default=1, help="worker processes to fork")
    parser.add_argument("--threads", type=int, default=4, help="application threads per worker")
    parser.add_argument(
        "--preload",
        choices=PRELOAD_MODES,
        default="auto",
        help="import in the host before forking: auto (with the import check), true or false",
    )
    parser.add_argument("--hooks", default=None, help="module providing the lifecycle hooks")
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
    return parser.parse_args(argv)


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
    if args.workers < 1:
        print("--workers must be at least 1", file=sys.stderr)
        return 2

    context = Context(app=app_name(args.app), process_model=process_model(args.threads))
    set_context(context)
    release = Release(args.app, args.hooks, context)

    # Section 6.2 step 1: no collections during preload.
    gc.disable()
    if args.preload != "false":
        started = time.monotonic()
        with preload.ImportRecorder() as recorder:
            try:
                hooks = release.load(recorder)
            except Exception as error:
                log(f"cannot load {args.app}: {error.__class__.__name__}: {error}")
                return 1
            if not hooks.call_logged("on_load", context):
                return 1
        log(f"preloaded {args.app} in {elapsed_ms(started)} ms")
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
    host = Host(release, args.socket_dir, args.workers, args.threads, args.tcp_base_port)
    return host.run()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
