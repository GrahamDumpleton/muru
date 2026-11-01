"""Hooks for the handoff fixture: register the application with a lifespan."""

from __future__ import annotations

from typing import Any

from handoff import with_lifespan

import muru.asgi


def post_fork(context: Any) -> None:
    muru.asgi.register(with_lifespan)
