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

HELLO = b"Hello, world!\n"
LARGE = b"x" * (1024 * 1024)

LOCAL = threading.local()
REQUEST_MARK: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "request_mark", default=None
)


def info() -> dict[str, Any]:
    is_gil_enabled = getattr(sys, "_is_gil_enabled", None)
    return {
        "pid": os.getpid(),
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

    if path == "/large":
        start_response(
            "200 OK",
            [("Content-Type", "application/octet-stream"), ("Content-Length", str(len(LARGE)))],
        )
        return [LARGE]

    if path == "/sleep":
        time.sleep(float(query.get("seconds", "0.1")))
        start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "5")])
        return [b"slept"]

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
