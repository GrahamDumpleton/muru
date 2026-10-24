"""The entrypoint named by the project's [tool.muru] table."""

from __future__ import annotations

import json
import os
import threading
from collections.abc import Callable, Iterable
from typing import Any

from muru import context


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    """Report the process, the thread count seen and the context."""

    body = json.dumps(
        {
            "pid": os.getpid(),
            "threads": threading.active_count(),
            "app": context().app,
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
