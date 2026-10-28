"""Hooks for the grouped project (ADR 0033).

A worker of the group that MURU_FAIL_GROUP names fails in post_fork, so a
test can see an optional group that cannot start leave the release ready.
"""

from __future__ import annotations

import os

from muru import Context


def post_fork(context: Context) -> None:
    """Fail the worker when the environment names its group."""

    if os.environ.get("MURU_FAIL_GROUP") == context.group:
        raise RuntimeError(f"the {context.group} group cannot start")
