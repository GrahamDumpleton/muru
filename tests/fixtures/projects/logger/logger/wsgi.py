"""The logger project's application: the logging fixture, as a project."""

from __future__ import annotations

import json
import logging
import os
import warnings
from collections.abc import Callable, Iterable
from typing import Any

log = logging.getLogger("demo")
noisy = logging.getLogger("noisy")


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    path = environ["PATH_INFO"]
    log.info("served %s", path)
    noisy.warning("noise for %s", path)

    if path == "/warn":
        warnings.warn("deprecated path", stacklevel=1)

    body = json.dumps(
        {
            "MURU_LOG_LEVEL": os.environ.get("MURU_LOG_LEVEL"),
            "MURU_LOG_FORMAT": os.environ.get("MURU_LOG_FORMAT"),
        }
    ).encode()
    start_response(
        "200 OK",
        [("Content-Type", "application/json"), ("Content-Length", str(len(body)))],
    )
    return [body]
