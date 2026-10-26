"""A plain WSGI application that reports on its process, for the phase 0 tests."""

from __future__ import annotations

import contextvars
import gc
import json
import os
import sys
import threading
import time
from collections.abc import Callable, Iterable
from typing import Any

import muru

HELLO = b"Hello, world!\n"
LARGE = b"x" * (1024 * 1024)

LOCAL = threading.local()

# What /grow keeps hold of, so the worker's memory stays up.

GROWN: list[bytes] = []
REQUEST_MARK: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "request_mark", default=None
)


def hold_the_interpreter(seconds: int) -> None:
    """Sleep in C without letting go of the interpreter, as a stuck
    extension would: no other thread of the worker runs Python meanwhile."""

    import ctypes
    import ctypes.util

    ctypes.PyDLL(ctypes.util.find_library("c")).sleep(seconds)


def umask_of_process() -> int:
    """Read the file creation mask without changing it for long."""

    current = os.umask(0)
    os.umask(current)
    return current


def info() -> dict[str, Any]:
    is_gil_enabled = getattr(sys, "_is_gil_enabled", None)
    return {
        "pid": os.getpid(),
        "uid": os.getuid(),
        "gid": os.getgid(),
        "umask": oct(umask_of_process()),
        "gc_enabled": gc.isenabled(),
        "gc_frozen": gc.get_freeze_count(),
        "threads": threading.active_count(),
        "free_threaded": bool(is_gil_enabled and not is_gil_enabled()),
        "python": sys.version.split()[0],
    }


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    path = environ["PATH_INFO"]
    query = dict(part.split("=", 1) for part in environ["QUERY_STRING"].split("&") if "=" in part)

    if path == "/":
        start_response(
            "200 OK",
            [("Content-Type", "text/plain"), ("Content-Length", str(len(HELLO)))],
        )
        return [HELLO]

    if path == "/info":
        body = json.dumps(
            {
                **info(),
                "environ": {
                    key: value
                    for key, value in environ.items()
                    if isinstance(value, (str, int, bool, tuple, dict))
                },
            }
        ).encode()
        start_response(
            "200 OK",
            [("Content-Type", "application/json"), ("Content-Length", str(len(body)))],
        )
        return [body]

    if path == "/echo":
        body = environ["wsgi.input"].read()
        start_response(
            "200 OK",
            [
                ("Content-Type", environ.get("CONTENT_TYPE", "application/octet-stream")),
                ("Content-Length", str(len(body))),
            ],
        )
        return [body]

    if path == "/lines":
        lines = list(environ["wsgi.input"])
        body = json.dumps([line.decode() for line in lines]).encode()
        start_response("200 OK", [("Content-Type", "application/json")])
        return [body]

    if path == "/chunks":
        count = int(query.get("n", "1000"))
        start_response("200 OK", [("Content-Type", "text/plain")])

        def generate() -> Iterable[bytes]:
            for index in range(count):
                yield b"%015d\n" % index

        return generate()

    if path == "/env":
        name = query.get("name", "")
        file_path = os.environ.get(f"{name}_FILE")
        content = None

        if file_path:
            with open(file_path, encoding="utf-8") as handle:
                content = handle.read()

        body = json.dumps(
            {"value": os.environ.get(name), "file": file_path, "content": content}
        ).encode()
        start_response(
            "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
        )
        return [body]

    if path == "/text":
        body = (b"The quick brown fox jumps over the lazy dog. " * 100)[:4096]
        headers = [
            ("Content-Type", "text/plain; charset=utf-8"),
            ("Content-Length", str(len(body))),
        ]
        if query.get("compress") == "off":
            headers.append(("X-Muru-Compress", "off"))
        if query.get("transform") == "no":
            headers.append(("Cache-Control", "no-transform"))
        if query.get("encoded") == "yes":
            headers.append(("Content-Encoding", "identity"))
        start_response("200 OK", headers)
        return [body]

    if path == "/large":
        start_response(
            "200 OK",
            [("Content-Type", "application/octet-stream"), ("Content-Length", str(len(LARGE)))],
        )
        return [LARGE]

    if path == "/grow":
        GROWN.append(b"x" * (int(query.get("mb", "1")) * 1024 * 1024))
        body = str(sum(len(chunk) for chunk in GROWN)).encode()
        start_response(
            "200 OK", [("Content-Type", "text/plain"), ("Content-Length", str(len(body)))]
        )
        return [body]

    if path == "/sleep":
        time.sleep(float(query.get("seconds", "0.1")))
        start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "5")])
        return [b"slept"]

    if path == "/hold":
        hold_the_interpreter(int(query.get("seconds", "1")))
        start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "4")])
        return [b"held"]

    if path == "/busy":
        # Runs Python bytecode for the time, so the soft timeout's
        # exception can be raised at any point.

        until = time.monotonic() + float(query.get("seconds", "0.1"))

        while time.monotonic() < until:
            time.sleep(0.01)

        start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "4")])
        return [b"busy"]

    if path == "/busy-handled":
        # An application that answers for itself when its request is
        # interrupted.

        until = time.monotonic() + float(query.get("seconds", "0.1"))

        try:
            while time.monotonic() < until:
                time.sleep(0.01)
        except muru.RequestTimeout:
            start_response(
                "503 Service Unavailable",
                [("Content-Type", "text/plain"), ("Content-Length", "14")],
            )
            return [b"took too long\n"]

        start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "4")])
        return [b"busy"]

    if path == "/events":
        # A server-sent event stream, which is exempt from the timeouts.

        count = int(query.get("count", "5"))
        delay = float(query.get("delay", "0.1"))
        start_response("200 OK", [("Content-Type", "text/event-stream")])

        def events() -> Iterable[bytes]:
            for index in range(count):
                yield b"data: %d\n\n" % index
                time.sleep(delay)

        return events()

    if path == "/error":
        raise RuntimeError("deliberate failure")

    if path == "/hsts":
        start_response(
            "200 OK",
            [
                ("Content-Type", "text/plain"),
                ("Content-Length", "2"),
                ("Strict-Transport-Security", "max-age=31536000"),
            ],
        )
        return [b"ok"]

    if path == "/threadlocal":
        LOCAL.count = getattr(LOCAL, "count", 0) + 1
        body = json.dumps({"count": LOCAL.count, "thread": threading.get_ident()}).encode()
        start_response(
            "200 OK",
            [("Content-Type", "application/json"), ("Content-Length", str(len(body)))],
        )
        return [body]

    if path == "/context":
        inherited = REQUEST_MARK.get()
        REQUEST_MARK.set("set by an earlier request")
        body = json.dumps({"inherited": inherited}).encode()
        start_response(
            "200 OK",
            [("Content-Type", "application/json"), ("Content-Length", str(len(body)))],
        )
        return [body]

    if path == "/ignore":
        start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "8")])
        return [b"ignored\n"]

    if path == "/write":
        write = start_response("200 OK", [("Content-Type", "text/plain")])
        write(b"written ")
        return [b"and yielded\n"]

    start_response("404 Not Found", [("Content-Type", "text/plain"), ("Content-Length", "10")])
    return [b"not found\n"]
