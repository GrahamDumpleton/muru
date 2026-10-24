"""Hooks that log every call, for the lifecycle tests."""

from __future__ import annotations

import os
import sys

from muru import Context


def _mark(name: str, context: Context) -> None:
    print(
        f"HOOK {name} pid={os.getpid()} worker={context.worker_id} app={context.app}",
        file=sys.stderr,
        flush=True,
    )


def on_load(context: Context) -> None:
    _mark("on_load", context)


def pre_fork(context: Context) -> None:
    _mark("pre_fork", context)


def post_fork(context: Context) -> None:
    _mark("post_fork", context)


def on_ready(context: Context) -> None:
    _mark("on_ready", context)


def on_drain(context: Context) -> None:
    _mark("on_drain", context)


def on_exit(context: Context) -> None:
    _mark("on_exit", context)


def on_reopen_logs(context: Context) -> None:
    _mark("on_reopen_logs", context)
