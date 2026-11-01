"""Call the callable a command declares (design specification section 9.12).

For a command declared with ``call = "module:function"``, the daemon runs
``python -m muru._command module:function`` with the release's interpreter,
environment and source directory, and passes the inputs as a JSON object in
``MURU_COMMAND_INPUTS``. This module imports the function and calls it with
the inputs as keyword arguments, awaiting it when it is a coroutine
function. An integer it returns is the exit status, anything else is 0, and
an exception it raises is printed with its traceback and exits 1.
"""

from __future__ import annotations

import asyncio
import importlib
import inspect
import json
import os
import sys
from typing import Any

__all__ = ["main"]


def resolve(target: str) -> Any:
    """Import ``module:function`` and return the function.

    The function may be a dotted path within the module, such as
    ``Class.method``.
    """

    module_name, _, attribute = target.partition(":")
    found: Any = importlib.import_module(module_name)

    for part in attribute.split("."):
        found = getattr(found, part)

    return found


def main(argv: list[str]) -> int:
    """Call the command's callable with its inputs and return the exit status."""

    if len(argv) != 2 or ":" not in argv[1]:
        print("usage: python -m muru._command module:function", file=sys.stderr)
        return 2

    # The inputs are taken out of the environment, so what the callable
    # starts does not inherit a secret input's value.

    inputs: dict[str, str] = json.loads(os.environ.pop("MURU_COMMAND_INPUTS", "{}"))
    function = resolve(argv[1])
    result = function(**inputs)

    if inspect.isawaitable(result):
        result = asyncio.run(_wait(result))

    if isinstance(result, int) and not isinstance(result, bool):
        return result

    return 0


async def _wait(awaitable: Any) -> Any:
    return await awaitable


if __name__ == "__main__":
    sys.exit(main(sys.argv))
