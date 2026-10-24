"""Hooks named by the project's [tool.muru] table."""

from __future__ import annotations

import os
from typing import Any


def on_load(context: Any) -> None:
    """Leave a mark the application can report."""

    os.environ["SHOP_HOOKS_LOADED"] = "yes"


def on_reopen_logs(context: Any) -> None:
    """Say so, for the logs tests."""

    print("shop: reopened logs", flush=True)
