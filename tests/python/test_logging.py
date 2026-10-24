"""Unit tests for the Python logging defaults (design specification section 11.4)."""

from __future__ import annotations

import io
import json
import logging
import logging.config
import os

import pytest

from muru import logging as muru_logging
from muru._context import Context
from muru._context import _set as set_context


def record(message: str, level: int = logging.WARNING) -> logging.LogRecord:
    return logging.LogRecord("demo", level, __file__, 1, message, None, None)


def test_standard_format_names_the_level_and_logger() -> None:
    assert muru_logging.formatter("standard").format(record("hello")) == "WARNING demo: hello"


def test_a_format_string_is_used_as_given() -> None:
    assert muru_logging.formatter("%(name)s|%(message)s").format(record("x")) == "demo|x"


def test_json_format_carries_the_muru_tags(monkeypatch: pytest.MonkeyPatch) -> None:
    set_context(Context(app="shop", release="r3", group="reports", worker_id=1))
    monkeypatch.setattr(
        "muru.logging.context",
        lambda: Context(app="shop", release="r3", group="reports", worker_id=1),
    )
    line = json.loads(muru_logging.formatter("json").format(record("hello")))
    assert line["level"] == "WARNING"
    assert line["logger"] == "demo"
    assert line["message"] == "hello"
    assert (line["app"], line["release"], line["group"], line["worker"]) == (
        "shop",
        "r3",
        "reports",
        1,
    )
    assert line["time"].endswith("Z")
    assert "exception" not in line


def test_json_format_includes_the_exception() -> None:
    try:
        raise ValueError("boom")
    except ValueError:
        failed = logging.LogRecord("demo", logging.ERROR, __file__, 1, "failed", None, None)
        import sys

        failed.exc_info = sys.exc_info()

    line = json.loads(muru_logging.formatter("json").format(failed))
    assert "ValueError: boom" in line["exception"]


def test_default_config_follows_the_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("MURU_LOG_LEVEL", raising=False)
    monkeypatch.delenv("MURU_LOG_FORMAT", raising=False)
    config = muru_logging.default_config()
    assert config["root"]["level"] == "WARNING"
    assert config["formatters"]["muru"]["format"] == "standard"

    monkeypatch.setenv("MURU_LOG_LEVEL", "DEBUG")
    monkeypatch.setenv("MURU_LOG_FORMAT", "json")
    config = muru_logging.default_config()
    assert config["root"]["level"] == "DEBUG"
    assert config["formatters"]["muru"]["format"] == "json"

    # The dictionary is accepted by dictConfig and builds the formatter.

    logging.config.dictConfig(config)
    handler = logging.getLogger().handlers[-1]
    assert isinstance(handler.formatter, muru_logging.JsonFormatter)
    logging.getLogger().removeHandler(handler)
    logging.getLogger().setLevel(logging.WARNING)


def test_install_replaces_last_resort_and_sets_levels() -> None:
    root = logging.getLogger()
    before = logging.lastResort
    root_level = root.level

    # pytest keeps a capturing handler on the root logger, which would
    # take the records before the last-resort handler is considered.

    handlers = root.handlers[:]
    root.handlers.clear()

    try:
        muru_logging._install("INFO", "standard", False, {"noisy": "ERROR"})
        assert logging.lastResort is not before
        assert root.level == logging.INFO
        assert logging.getLogger("noisy").level == logging.ERROR
        assert os.environ["MURU_LOG_LEVEL"] == "INFO"
        assert os.environ["MURU_LOG_FORMAT"] == "standard"

        # A logger without handlers goes through the last-resort handler in
        # the platform format, at the root's level.

        stream = io.StringIO()
        assert isinstance(logging.lastResort, logging.StreamHandler)
        logging.lastResort.setStream(stream)
        logging.getLogger("fresh.logger").info("shown")
        logging.getLogger("fresh.logger").debug("below the level")
        logging.getLogger("noisy").warning("below its own level")
        assert stream.getvalue() == "INFO fresh.logger: shown\n"
    finally:
        logging.lastResort = before
        root.setLevel(root_level)
        root.handlers[:] = handlers
        logging.getLogger("noisy").setLevel(logging.NOTSET)
