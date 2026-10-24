"""A WSGI application that logs through the standard library, for the
logging defaults tests: no handlers are configured, so what is shown is
what the platform's last-resort handler does."""

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
    log.debug("debug for %s", path)
    log.info("served %s", path)
    log.warning("warned for %s", path)
    noisy.warning("noise for %s", path)

    if path == "/warn":
        warnings.warn("deprecated path", stacklevel=1)

    if path == "/fail":
        try:
            raise ValueError("boom")
        except ValueError:
            log.exception("failed for %s", path)

    body = json.dumps(
        {
            "MURU_LOG_LEVEL": os.environ.get("MURU_LOG_LEVEL"),
            "MURU_LOG_FORMAT": os.environ.get("MURU_LOG_FORMAT"),
            "root_level": logging.getLevelName(logging.getLogger().level),
            "noisy_level": logging.getLevelName(noisy.level),
        }
    ).encode()
    start_response(
        "200 OK",
        [("Content-Type", "application/json"), ("Content-Length", str(len(body)))],
    )
    return [body]
