"""The entrypoint named by the project's [tool.muru] table."""

from __future__ import annotations

import json
import os
import threading
import time
from collections.abc import Callable, Iterable
from typing import Any

from muru import context, is_internal


def redirecting(
    environ: dict[str, Any], start_response: Callable[..., Any]
) -> Iterable[bytes] | None:
    """The routes that answer with internal redirects, or report arriving by one.

    They are matched on the last path segment, so the same code serves
    under any route prefix. None means the path is not one of them.
    """

    name = environ["PATH_INFO"].rsplit("/", 1)[-1]
    query = environ.get("QUERY_STRING", "")
    params = dict(part.split("=", 1) for part in query.split("&") if "=" in part)

    if name == "download":
        # An authorised download: the edge serves the protected file with
        # the disposition and cache headers this response names.

        file = params.get("file", "fox.txt")
        start_response(
            "200 OK",
            [
                ("X-Muru-Redirect", f"/protected/{file}"),
                ("Content-Disposition", f'attachment; filename="{file}"'),
                ("Cache-Control", "private"),
                ("Content-Length", "0"),
            ],
        )
        return [b""]

    if name == "redirect-to":
        headers = [("X-Muru-Redirect", params.get("path", "/internal-info"))]

        if "host" in params:
            headers.append(("X-Muru-Redirect-Host", params["host"]))

        if params.get("method") == "preserve":
            headers.append(("X-Muru-Redirect-Method", "preserve"))

        for item in params.get("header", "").split(","):
            if ":" in item:
                header, value = item.split(":", 1)
                headers.append((f"X-Muru-Redirect-Header-{header}", value))

        # A body too, which the edge drains before it routes the request on.

        start_response(
            f"{params.get('status', '200')} Redirecting",
            [*headers, ("Content-Type", "text/plain"), ("Content-Length", "9")],
        )
        return [b"redirect\n"]

    if name == "accel":
        start_response(
            "200 OK",
            [("X-Accel-Redirect", params.get("path", "/internal-info")), ("Content-Length", "0")],
        )
        return [b""]

    if name == "cgi":
        start_response(
            "200 OK", [("Location", params.get("path", "/internal-info")), ("Content-Length", "0")]
        )
        return [b""]

    if name == "loop":
        start_response(
            "200 OK", [("X-Muru-Redirect", environ["PATH_INFO"]), ("Content-Length", "0")]
        )
        return [b""]

    if name == "internal-info":
        body = json.dumps(
            {
                "app": context().app,
                "internal": is_internal(environ),
                "method": environ["REQUEST_METHOD"],
                "path": environ["PATH_INFO"],
                "query": query,
                "redirect_url": environ.get("REDIRECT_URL"),
                "redirect_query_string": environ.get("REDIRECT_QUERY_STRING"),
                "redirect_status": environ.get("REDIRECT_STATUS"),
                "x_user": environ.get("HTTP_X_USER"),
                "x_muru_secret": environ.get("HTTP_X_MURU_SECRET"),
                "request_id": environ.get("muru.request_id"),
                "body": environ["wsgi.input"].read().decode(),
            }
        ).encode()
        start_response(
            "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
        )
        return [body]

    return None


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    """Report the process, the thread count seen and the context.

    Each request also prints a line, which the host relays into the
    release's log for the logs tests.
    """

    print(f"shop: served {environ['PATH_INFO']}", flush=True)

    if environ["PATH_INFO"] == "/error":
        raise RuntimeError("deliberate failure")

    redirected = redirecting(environ, start_response)

    if redirected is not None:
        return redirected

    # /slow?ms=N holds the worker's thread for that long, for the scaling
    # and timeout tests.

    if environ["PATH_INFO"] == "/slow":
        query = environ.get("QUERY_STRING", "")
        held_ms = int(query.partition("=")[2] or "500") if query.startswith("ms=") else 500
        time.sleep(held_ms / 1000)

    body = json.dumps(
        {
            "pid": os.getpid(),
            "threads": threading.active_count(),
            "app": context().app,
            "release": context().release,
            "group": context().group,
            "worker": context().worker_id,
            "hooks_loaded": os.environ.get("SHOP_HOOKS_LOADED", ""),
            "path": environ["PATH_INFO"],
        }
    ).encode()
    start_response(
        "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
    )
    return [body]
