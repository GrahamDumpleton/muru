"""Python logging defaults (design specification section 11.4).

Before the application is imported, the host sets the root logger's level
and replaces ``logging.lastResort`` with a handler that writes to standard
error in the configured format, so a logger with no handler of its own is
shown the platform way. ``basicConfig`` and ``dictConfig`` are untouched:
the last-resort handler is used only when no handler exists in a logger's
hierarchy, so an application that configures logging behaves exactly as
it does without Muru. ``default_config()`` is the same defaults as a
dictConfig dictionary, for applications that want to adopt them
explicitly.
"""

from __future__ import annotations

import datetime
import json
import logging
import os
import sys
from typing import Any

from muru._context import context

__all__ = ["STANDARD_FORMAT", "JsonFormatter", "default_config", "formatter"]

STANDARD_FORMAT = "%(levelname)s %(name)s: %(message)s"
"""The ``standard`` format: the level, the logger and the message."""


class JsonFormatter(logging.Formatter):
    """One JSON object per record, with the Muru tags of the process."""

    def format(self, record: logging.LogRecord) -> str:
        """Format the record as one JSON object."""

        where = context()
        line: dict[str, Any] = {
            "time": datetime.datetime.fromtimestamp(record.created, datetime.UTC).isoformat(
                timespec="milliseconds"
            )[:-6]
            + "Z",
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
            "app": where.app,
            "release": where.release,
            "group": where.group,
            "worker": where.worker_id,
        }

        if record.exc_info and record.exc_info[0] is not None:
            line["exception"] = self.formatException(record.exc_info)

        if record.stack_info:
            line["stack"] = self.formatStack(record.stack_info)

        return json.dumps(line, default=str)


def formatter(format: str = "standard") -> logging.Formatter:
    """Return the formatter for a format name.

    ``standard`` and ``json`` are the platform formats; anything else is
    a ``logging`` format string.
    """

    if format == "standard":
        return logging.Formatter(STANDARD_FORMAT)

    if format == "json":
        return JsonFormatter()

    return logging.Formatter(format)


class _FallbackHandler(logging.StreamHandler):  # type: ignore[type-arg]
    """A handler that writes only when nothing else in the hierarchy would.

    ``logging.captureWarnings`` gives the ``py.warnings`` logger a
    ``NullHandler``, which stops the last-resort handler from ever seeing
    a warning; this handler stands in for it there, and steps aside once
    the application configures handlers of its own.
    """

    def emit(self, record: logging.LogRecord) -> None:
        """Write the record unless another handler would."""

        logger: logging.Logger | None = logging.getLogger(record.name)

        while logger is not None:
            for handler in logger.handlers:
                if handler is not self and not isinstance(handler, logging.NullHandler):
                    return

            if not logger.propagate:
                break

            logger = logger.parent

        super().emit(record)


def default_config() -> dict[str, Any]:
    """Return the platform defaults as a ``logging.config.dictConfig`` dictionary.

    The root level and the format come from MURU_LOG_LEVEL and
    MURU_LOG_FORMAT, which the host sets from the release's settings;
    outside Muru they are WARNING and ``standard``.
    """

    return {
        "version": 1,
        "disable_existing_loggers": False,
        "formatters": {
            "muru": {
                "()": "muru.logging.formatter",
                "format": os.environ.get("MURU_LOG_FORMAT", "standard"),
            }
        },
        "handlers": {
            "muru": {
                "class": "logging.StreamHandler",
                "stream": "ext://sys.stderr",
                "formatter": "muru",
            }
        },
        "root": {
            "level": os.environ.get("MURU_LOG_LEVEL", "WARNING"),
            "handlers": ["muru"],
        },
    }


def _install(
    level: str | None,
    format: str | None,
    capture_warnings: bool,
    loggers: dict[str, str],
) -> None:
    """Apply the release's logging settings before the application is imported.

    The host calls this once; the workers inherit the result, and the
    environment variables tell the application what was chosen.
    """

    os.environ["MURU_LOG_LEVEL"] = level or "WARNING"
    os.environ["MURU_LOG_FORMAT"] = format or "standard"
    handler = logging.StreamHandler(sys.stderr)
    handler.setFormatter(formatter(format or "standard"))
    logging.lastResort = handler

    if level:
        logging.getLogger().setLevel(level)

    for name, named_level in loggers.items():
        logging.getLogger(name).setLevel(named_level)

    if capture_warnings:
        logging.captureWarnings(True)
        fallback = _FallbackHandler(sys.stderr)
        fallback.setFormatter(formatter(format or "standard"))
        logging.getLogger("py.warnings").addHandler(fallback)
