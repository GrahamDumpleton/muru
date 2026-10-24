"""Lifecycle hooks from the module named by the hooks setting (section 6.3).

Each hook is an optional module-level function taking the ``Context``:
on_load and pre_fork run in the host after the import, post_fork, on_ready,
on_drain and on_exit in each worker, and on_reopen_logs in a worker that
receives SIGUSR1. With per-worker imports, on_load and pre_fork run in each
worker before its post_fork.
"""

from __future__ import annotations

import importlib
import sys
import traceback
from collections.abc import Callable
from typing import Any

from muru._context import Context

__all__ = ["HOOKS", "Hooks"]

HOOKS = (
    "on_load",
    "pre_fork",
    "post_fork",
    "on_ready",
    "on_drain",
    "on_exit",
    "on_reopen_logs",
)

Hook = Callable[[Context], Any]


class Hooks:
    """The hook functions found in a hooks module, or none at all."""

    def __init__(self, functions: dict[str, Hook] | None = None, module: str | None = None) -> None:
        self.functions = functions or {}
        self.module = module

    @classmethod
    def load(cls, name: str | None) -> Hooks:
        """Import the hooks module and collect its hook functions.

        Raises ImportError when the module cannot be imported and TypeError
        when an attribute with a hook's name is not callable.
        """

        if not name:
            return cls()
        module = importlib.import_module(name)
        functions: dict[str, Hook] = {}

        for hook in HOOKS:
            function = getattr(module, hook, None)
            if function is None:
                continue
            if not callable(function):
                raise TypeError(f"{name}.{hook} is not callable")
            functions[hook] = function

        return cls(functions, name)

    def has(self, hook: str) -> bool:
        """Whether the module defines this hook."""

        return hook in self.functions

    def call(self, hook: str, context: Context) -> None:
        """Run a hook if it is defined. Whatever it raises propagates."""

        function = self.functions.get(hook)
        if function is not None:
            function(context)

    def call_logged(self, hook: str, context: Context) -> bool:
        """Run a hook, reporting an exception to stderr instead of raising it.

        Returns whether the hook completed.
        """

        try:
            self.call(hook, context)
        except BaseException:
            print(f"muru: the {hook} hook failed", file=sys.stderr)
            traceback.print_exc()
            sys.stderr.flush()
            return False

        return True
