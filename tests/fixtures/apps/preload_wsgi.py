"""Reports which process imported it, for the preload and hook tests."""

from __future__ import annotations

import json
import os
from collections.abc import Callable, Iterable
from typing import Any

IMPORT_PID = os.getpid()


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    import muru

    context = muru.context()
    body = json.dumps(
        {
            "import_pid": IMPORT_PID,
            "pid": os.getpid(),
            "worker_id": context.worker_id,
            "app": context.app,
            "process_model": context.process_model,
        }
    ).encode()
    start_response(
        "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
    )
    return [body]
