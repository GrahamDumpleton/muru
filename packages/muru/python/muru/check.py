"""Load an application the way the host would and report what was found.

``muructl check`` runs this module in the application's interpreter
(design specification section 6.8): the hooks module and the entrypoint
are imported under the import check of section 6.2, the entrypoint is
settled as WSGI, ASGI or gRPC with the signature rules the host applies,
and the mounts are loaded the same way. Nothing is served and nothing is
forked. The result is one JSON document on stdout, for muructl to report.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import traceback
from typing import Any

from muru import __version__, host, preload
from muru.hooks import Hooks

__all__ = ["main"]


def load(
    spec: str,
    interface: str,
    factory: bool,
    factory_phase: str,
    recorder: preload.ImportRecorder,
) -> dict[str, Any]:
    """Import one entrypoint and settle its interface, as the host does."""

    target = host.resolve_app(spec, recorder.import_module)

    if factory and factory_phase != "host":
        # A per-worker factory is called in each worker after fork; here
        # it is only known to be callable.

        return {"entrypoint": spec, "interface": interface, "factory": "worker"}

    app = target() if factory else target
    resolved = host.resolve_interface(spec, app, interface)
    return {"entrypoint": spec, "interface": resolved, "factory": "host" if factory else None}


def parse_mount(text: str) -> tuple[str, str, str]:
    """Read ``NAME=ENTRYPOINT=INTERFACE``."""

    parts = text.split("=")

    if len(parts) != 3:
        raise argparse.ArgumentTypeError(f"{text!r} is not NAME=ENTRYPOINT=INTERFACE")

    return parts[0], parts[1], parts[2]


def main(argv: list[str] | None = None) -> int:
    """Load the application and print the report."""

    parser = argparse.ArgumentParser(prog="python -m muru.check")
    parser.add_argument("--app", required=True, help="application as module:callable")
    parser.add_argument(
        "--chdir", default=None, help="directory to run in, added to the module search path"
    )
    parser.add_argument("--interface", default="auto", choices=["auto", "wsgi", "asgi", "grpc"])
    parser.add_argument("--factory", action="store_true", help="the entrypoint is a factory")
    parser.add_argument("--factory-phase", default="worker", choices=["worker", "host"])
    parser.add_argument("--hooks", default=None, help="module providing the lifecycle hooks")
    parser.add_argument(
        "--mount",
        action="append",
        default=[],
        type=parse_mount,
        help="a named mount as NAME=ENTRYPOINT=INTERFACE; repeatable",
    )
    args = parser.parse_args(argv)

    if args.chdir:
        os.chdir(args.chdir)
        sys.path.insert(0, args.chdir)

    report: dict[str, Any] = {
        "ok": True,
        "python": ".".join(str(part) for part in sys.version_info[:3]),
        "muru": __version__,
        "entrypoints": [],
        "hooks": [],
        "findings": [],
        "errors": [],
    }

    with preload.ImportRecorder() as recorder:
        try:
            if args.hooks:
                recorder.import_module(args.hooks)
                hooks = Hooks.load(args.hooks)
                report["hooks"] = sorted(hooks.functions)

            report["entrypoints"].append(
                load(args.app, args.interface, args.factory, args.factory_phase, recorder)
            )

            for name, entrypoint, interface in args.mount:
                loaded = load(entrypoint, interface, False, "worker", recorder)
                loaded["mount"] = name
                report["entrypoints"].append(loaded)
        except Exception as error:
            report["ok"] = False
            report["errors"].append(
                {
                    "error": f"{error.__class__.__name__}: {error}",
                    "traceback": traceback.format_exc(),
                }
            )

    report["findings"] = [
        {
            "kind": finding.kind,
            "detail": finding.detail,
            "module": finding.module,
            "description": finding.describe(),
            "advice": finding.advice,
        }
        for finding in recorder.findings()
    ]
    json.dump(report, sys.stdout)
    sys.stdout.write("\n")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
