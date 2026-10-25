"""The entrypoint named by the project's [tool.muru] table."""

from __future__ import annotations

import json
import os
import threading
import time
from collections.abc import Callable, Iterable
from typing import Any

from muru import context


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    """Report the process, the thread count seen and the context.

    Each request also prints a line, which the host relays into the
    release's log for the logs tests.
    """

    print(f"shop: served {environ['PATH_INFO']}", flush=True)

    if environ["PATH_INFO"] == "/error":
        raise RuntimeError("deliberate failure")

    # /slow?ms=N holds the worker's thread for that long, for the scaling
    # and timeout tests.

    if environ["PATH_INFO"] == "/slow":
        query = environ.get("QUERY_STRING", "")
        held_ms = int(query.partition("=")[2] or "500") if query.startswith("ms=") else 500
        time.sleep(held_ms / 1000)

    body = json.dumps(
        {
            "pid": os.getpid(),
            "threads": threading.active_count(),
            "app": context().app,
            "release": context().release,
            "group": context().group,
            "worker": context().worker_id,
            "hooks_loaded": os.environ.get("SHOP_HOOKS_LOADED", ""),
            "path": environ["PATH_INFO"],
        }
    ).encode()
    start_response(
        "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
    )
    return [body]
