"""A project whose muru.toml declares worker groups (section 7.1).

The response says which group and worker served it, and the group the
environment names, so a test can see paths reach the right workers. A
query string of ms=N holds the request for that long, for the group
timeouts.
"""

from __future__ import annotations

import json
import os
import time
from collections.abc import Callable, Iterable
from typing import Any

from muru import context


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    """Report the group, the worker and the path."""

    query = environ.get("QUERY_STRING", "")

    if query.startswith("ms="):
        time.sleep(int(query[3:]) / 1000)

    body = json.dumps(
        {
            "group": context().group,
            "env_group": os.environ.get("MURU_GROUP"),
            "worker": context().worker_id,
            "pid": os.getpid(),
            "path": environ["PATH_INFO"],
        }
    ).encode()
    start_response(
        "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
    )

    return [body]
