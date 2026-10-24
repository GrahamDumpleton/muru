"""A project whose muru.toml declares worker groups (section 7.1).

The response says which group and worker served it, and the group the
environment names, so a test can see paths reach the right workers.
"""

from __future__ import annotations

import json
import os
from collections.abc import Callable, Iterable
from typing import Any

from muru import context


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    """Report the group, the worker and the path."""

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
