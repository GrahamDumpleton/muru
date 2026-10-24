"""A hooks module whose on_load fails, for the lifecycle tests."""

from __future__ import annotations

from muru import Context


def on_load(context: Context) -> None:
    raise RuntimeError("load-boom")
