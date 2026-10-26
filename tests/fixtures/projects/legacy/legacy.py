"""A WSGI application for gunicorn, which reports the server it runs under.

`native` says whether Muru's own workers built the environ, for the
cutover from gunicorn to native workers.
"""

from __future__ import annotations

import json
import os
from collections.abc import Callable, Iterable
from typing import Any


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    body = json.dumps(
        {
            "server": "gunicorn",
            "pid": os.getpid(),
            "path": environ["PATH_INFO"],
            "forwarded_for": environ.get("HTTP_X_FORWARDED_FOR", ""),
            "native": "muru.client_class" in environ,
        }
    ).encode()
    start_response(
        "200 OK",
        [("Content-Type", "application/json"), ("Content-Length", str(len(body)))],
    )
    return [body]
