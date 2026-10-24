"""The import check: what preloading left behind that fork would break.

After the host has imported the application (design specification section
6.2), the process should still be single-threaded with nothing connected:
a socket or pipe opened at import time would be shared by every worker, and
a thread or child process does not survive fork at all. ``ImportRecorder``
compares the process before and after preload, and watches each first
import on the way so that a finding can name the module that caused it.
"""

from __future__ import annotations

import builtins
import ctypes
import ctypes.util
import importlib
import os
import socket
import stat
import sys
import threading
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from importlib.util import resolve_name
from types import ModuleType, TracebackType
from typing import Any

__all__ = ["Finding", "ImportRecorder", "child_pids", "thread_count"]

# Compared through a name rather than sys.platform itself, so that mypy on
# one platform does not report the other platform's branch as unreachable.

_PLATFORM = sys.platform


@dataclass(frozen=True)
class Finding:
    """One thing the import left behind that cannot cross fork safely."""

    kind: str
    """socket, pipe, thread or child."""

    detail: str
    """What was found, for example "a connected socket (fd 7, to 127.0.0.1:5432)"."""

    module: str | None = None
    """The module whose import created it, when that could be determined."""

    def describe(self) -> str:
        """One sentence naming the finding and, when known, its module."""

        origin = f"the import of {self.module}" if self.module else "the import"
        return f"{origin} left {self.detail}"

    @property
    def advice(self) -> str:
        """What to change so that the application preloads cleanly."""

        if self.kind in ("socket", "pipe"):
            return (
                "close it in a pre_fork hook and reopen it in post_fork, "
                "or open it lazily on first use"
            )
        return "start it from a post_fork hook instead"


# --- threads and children -------------------------------------------------

_PROC_PIDTASKINFO = 4
_TASKINFO_SIZE = 96
_THREADNUM_OFFSET = 84
_libproc_handle: ctypes.CDLL | bool | None = False


def _libproc() -> ctypes.CDLL | None:
    """Load the macOS libproc library once; None when it is unavailable."""

    global _libproc_handle
    if _libproc_handle is False:
        _libproc_handle = None
        path = ctypes.util.find_library("proc")
        if path is not None:
            try:
                library = ctypes.CDLL(path)
                library.proc_pidinfo.restype = ctypes.c_int
                library.proc_pidinfo.argtypes = [
                    ctypes.c_int,
                    ctypes.c_int,
                    ctypes.c_uint64,
                    ctypes.c_void_p,
                    ctypes.c_int,
                ]
                library.proc_listchildpids.restype = ctypes.c_int
                library.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
                _libproc_handle = library
            except (OSError, AttributeError):
                _libproc_handle = None
    return _libproc_handle if isinstance(_libproc_handle, ctypes.CDLL) else None


def thread_count() -> int:
    """Count the threads of this process, including ones Python cannot see.

    Native threads started by C libraries do not appear in the threading
    module, so the count comes from the operating system where possible:
    /proc on Linux and the task information on macOS.
    """

    if _PLATFORM == "linux":
        try:
            return len(os.listdir("/proc/self/task"))
        except OSError:
            pass
    elif _PLATFORM == "darwin":
        library = _libproc()
        if library is not None:
            buffer = ctypes.create_string_buffer(_TASKINFO_SIZE)
            size = library.proc_pidinfo(os.getpid(), _PROC_PIDTASKINFO, 0, buffer, _TASKINFO_SIZE)
            if size >= _TASKINFO_SIZE:
                raw = buffer.raw[_THREADNUM_OFFSET : _THREADNUM_OFFSET + 4]
                return int.from_bytes(raw, sys.byteorder, signed=True)
    return threading.active_count()


def child_pids() -> set[int]:
    """Return the pids of this process's live children."""

    pid = os.getpid()
    if _PLATFORM == "linux":
        children: set[int] = set()
        try:
            for task in os.listdir("/proc/self/task"):
                with open(f"/proc/self/task/{task}/children", encoding="ascii") as handle:
                    children.update(int(child) for child in handle.read().split())
            return children
        except OSError:
            pass
        try:
            for entry in os.listdir("/proc"):
                if not entry.isdigit():
                    continue
                with open(f"/proc/{entry}/stat", encoding="ascii", errors="replace") as handle:
                    fields = handle.read().rpartition(")")[2].split()
                if len(fields) > 1 and int(fields[1]) == pid:
                    children.add(int(entry))
        except OSError:
            pass
        return children
    if _PLATFORM == "darwin":
        library = _libproc()
        if library is not None:
            buffer = (ctypes.c_int * 4096)()
            count = library.proc_listchildpids(pid, buffer, ctypes.sizeof(buffer))
            if count >= 0:
                return {int(buffer[index]) for index in range(min(count, len(buffer)))}
    return set()


# --- descriptors ------------------------------------------------------------

Identity = tuple[int, int, int]


def _fd_directory() -> str:
    return "/proc/self/fd" if _PLATFORM == "linux" else "/dev/fd"


def open_fds() -> dict[int, Identity]:
    """Every open descriptor with its (mode, device, inode) identity."""

    found: dict[int, Identity] = {}
    try:
        entries = os.listdir(_fd_directory())
    except OSError:
        return found
    for entry in entries:
        if not entry.isdigit():
            continue
        fd = int(entry)
        try:
            info = os.fstat(fd)
        except OSError:
            # The descriptor used to list the directory, already closed.

            continue
        found[fd] = (stat.S_IFMT(info.st_mode), info.st_dev, info.st_ino)
    return found


def _fd_numbers() -> set[int]:
    """Every open descriptor number.

    Listing the descriptor directory opens a descriptor of its own, which
    is closed again by the time the listing is examined; checking each
    entry is still open leaves it out, so the number is free to be reused
    by whatever the import opens next.
    """

    return set(open_fds())


def _format_peer(peer: Any) -> str:
    if isinstance(peer, tuple) and len(peer) >= 2:
        host, port = peer[0], peer[1]
        return f"[{host}]:{port}" if ":" in str(host) else f"{host}:{port}"
    if isinstance(peer, bytes):
        return repr(peer)
    return str(peer) or "an unnamed peer"


def _peer_name(fd: int) -> str | None:
    """Return the peer of a connected socket, or None when it is not connected."""

    try:
        duplicate = os.dup(fd)
    except OSError:
        return None
    try:
        sock = socket.socket(fileno=duplicate)
    except OSError:
        os.close(duplicate)
        return None
    try:
        peer = sock.getpeername()
    except OSError:
        return None
    finally:
        sock.close()
    return _format_peer(peer)


def _classify(fd: int, mode: int, children_appeared: bool) -> tuple[str, str] | None:
    """Kind and detail for a descriptor that workers must not inherit, else None.

    Regular files, unconnected sockets and pipes with no child process on
    the other end are safe to share and are not reported.
    """

    if stat.S_ISSOCK(mode):
        peer = _peer_name(fd)
        if peer is None:
            return None
        return "socket", f"a connected socket (fd {fd}, to {peer})"
    if stat.S_ISFIFO(mode) and children_appeared:
        return "pipe", f"a pipe (fd {fd}) to a child process"
    return None


# --- the recorder -----------------------------------------------------------

ImportFunction = Callable[..., ModuleType]


class ImportRecorder:
    """Watch what preload leaves behind, attributing it to the imports that did it.

    Use as a context manager around the import of the application and its
    on_load hook, then read ``findings()``. While active, the builtin
    ``__import__`` is wrapped so that each module's first import is checked
    for new descriptors and threads; ``import_module`` does the same for an
    import made through importlib.
    """

    def __init__(self) -> None:
        self._fds_before: dict[int, Identity] = {}
        self._threads_before = 0
        self._children_before: set[int] = set()
        self._fd_owner: dict[int, str] = {}
        self._thread_owners: list[tuple[str, int]] = []
        self._original_import: ImportFunction | None = None

    def __enter__(self) -> ImportRecorder:
        self._fds_before = open_fds()
        self._threads_before = thread_count()
        self._children_before = child_pids()
        self._original_import = builtins.__import__
        builtins.__import__ = self._import
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        if self._original_import is not None:
            builtins.__import__ = self._original_import
            self._original_import = None

    def import_module(self, name: str) -> ModuleType:
        """Import a module through importlib, attributing what it leaves to it."""

        return self._record(name, lambda: importlib.import_module(name))

    def _import(
        self,
        name: str,
        globals: Mapping[str, object] | None = None,
        locals: Mapping[str, object] | None = None,
        fromlist: Any = (),
        level: int = 0,
    ) -> ModuleType:
        original = self._original_import or builtins.__import__
        full_name = name
        if level > 0:
            package = globals.get("__package__") if globals else None
            try:
                full_name = resolve_name("." * level + name, str(package or ""))
            except (ImportError, ValueError):
                full_name = name
        if full_name in sys.modules:
            return original(name, globals, locals, fromlist, level)
        return self._record(full_name, lambda: original(name, globals, locals, fromlist, level))

    def _record(self, module: str, run: Callable[[], ModuleType]) -> ModuleType:
        fds_before = _fd_numbers()
        threads_before = thread_count()
        try:
            return run()
        finally:
            for fd in _fd_numbers() - fds_before:
                self._fd_owner.setdefault(fd, module)
            grown = thread_count() - threads_before
            if grown > 0:
                self._thread_owners.append((module, grown))

    def findings(self) -> list[Finding]:
        """Return what is still left behind now, compared with before the imports."""

        findings: list[Finding] = []
        children = child_pids() - self._children_before
        for fd, identity in sorted(open_fds().items()):
            if self._fds_before.get(fd) == identity:
                continue
            classified = _classify(fd, identity[0], bool(children))
            if classified is not None:
                kind, detail = classified
                findings.append(Finding(kind, detail, self._fd_owner.get(fd)))
        extra_threads = thread_count() - self._threads_before
        if extra_threads > 0:
            attributed = 0
            for module, grown in self._thread_owners:
                count = min(grown, extra_threads - attributed)
                if count <= 0:
                    break
                attributed += count
                findings.append(Finding("thread", _threads(count), module))
            if attributed < extra_threads:
                findings.append(Finding("thread", _threads(extra_threads - attributed)))
        for pid in sorted(children):
            findings.append(Finding("child", f"a child process running (pid {pid})"))
        return findings


def _threads(count: int) -> str:
    return "a thread running" if count == 1 else f"{count} threads running"
