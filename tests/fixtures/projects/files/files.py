"""A project that hands files to the edge (section 4.9).

The default entrypoint is WSGI and returns `wsgi.file_wrapper` objects or
`X-Muru-Sendfile` headers for files under the directory `FILES_DIR` names;
an ASGI mount sends the `pathsend` and `zerocopysend` extensions; a second
WSGI mount sends `X-Sendfile` for the mount's compatibility header; and a
Starlette mount serves `FileResponse`, which uses `pathsend` when the
server advertises it. The test writes the files and the project's
`muru.toml`, whose `sendfile_roots` names the same directory.
"""

from __future__ import annotations

import io
import json
import os
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs

FILES_DIR = Path(os.environ.get("FILES_DIR", "/nonexistent"))


class Tracked:
    """A file object that announces its closing in the worker's log."""

    def __init__(self, path: Path) -> None:
        self.handle = path.open("rb")
        self.name = path.name

    def fileno(self) -> int:
        return self.handle.fileno()

    def tell(self) -> int:
        return self.handle.tell()

    def seek(self, offset: int) -> int:
        return self.handle.seek(offset)

    def read(self, size: int = -1) -> bytes:
        return self.handle.read(size)

    def close(self) -> None:
        self.handle.close()
        print(f"MARKER_CLOSED {self.name}", flush=True)


def wsgi_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    path = environ["PATH_INFO"]
    query = parse_qs(environ["QUERY_STRING"])
    segments = path.strip("/").split("/")
    kind = segments[0]
    name = "/".join(segments[1:])
    wrapper = environ["wsgi.file_wrapper"]

    if kind == "wrapper":
        start_response("200 OK", [("Content-Type", "application/octet-stream")])
        return wrapper(Tracked(FILES_DIR / name), 8192)

    if kind == "wrapper-range":
        tracked = Tracked(FILES_DIR / name)
        tracked.seek(int(query["offset"][0]))
        start_response(
            "200 OK",
            [("Content-Type", "application/octet-stream"), ("Content-Length", query["length"][0])],
        )
        return wrapper(tracked, 8192)

    if kind == "wrapper-bytesio":
        start_response("200 OK", [("Content-Type", "text/plain")])
        return wrapper(io.BytesIO(b"from memory"), 4)

    if kind == "wrapper-after-write":
        write = start_response("200 OK", [("Content-Type", "application/octet-stream")])
        write(b"prefix:")
        return wrapper(Tracked(FILES_DIR / name), 8192)

    if kind == "sendfile":
        start_response(
            "200 OK",
            [
                ("Content-Type", "text/x-custom"),
                ("X-App", "yes"),
                ("X-Muru-Sendfile", str(FILES_DIR / name)),
            ],
        )
        return [b"the application's own body, never sent"]

    if kind == "sendfile-path":
        start_response("200 OK", [("X-Muru-Sendfile", query["path"][0])])
        return [b"never sent"]

    if kind == "sendfile-status":
        start_response("201 Created", [("X-Muru-Sendfile", str(FILES_DIR / name))])
        return [b"never sent"]

    if kind == "x-sendfile":
        start_response("200 OK", [("X-Sendfile", str(FILES_DIR / name))])
        return [b"the application's own body"]

    start_response("404 Not Found", [("Content-Type", "text/plain")])
    return [b"unknown test path\n"]


def compat_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    """The mount whose `sendfile_header` is `X-Sendfile`."""

    name = environ["PATH_INFO"].strip("/")
    start_response("200 OK", [("X-Sendfile", str(FILES_DIR / name))])
    return [b"the application's own body"]


async def asgi_app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    if scope["type"] == "lifespan":
        while True:
            message = await receive()

            if message["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            elif message["type"] == "lifespan.shutdown":
                await send({"type": "lifespan.shutdown.complete"})
                return

    path = scope["path"].removeprefix(scope["root_path"])
    query = parse_qs(scope["query_string"].decode())
    segments = path.strip("/").split("/")
    kind = segments[0]
    name = segments[1] if len(segments) > 1 else ""

    if kind == "extensions":
        body = json.dumps(sorted(scope.get("extensions", {}))).encode()
        await send(
            {
                "type": "http.response.start",
                "status": 200,
                "headers": [(b"content-type", b"application/json")],
            }
        )
        await send({"type": "http.response.body", "body": body})
        return

    if kind == "pathsend":
        await send(
            {
                "type": "http.response.start",
                "status": 200,
                "headers": [(b"content-type", b"application/octet-stream")],
            }
        )
        await send({"type": "http.response.pathsend", "path": str(FILES_DIR / name)})
        return

    if kind == "zerocopysend":
        with (FILES_DIR / name).open("rb") as handle:
            message: dict[str, Any] = {"type": "http.response.zerocopysend", "file": handle}

            if "offset" in query:
                message["offset"] = int(query["offset"][0])

            if "count" in query:
                message["count"] = int(query["count"][0])

            await send(
                {
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"application/octet-stream")],
                }
            )
            await send(message)

        return

    if kind == "mixed":
        with (FILES_DIR / name).open("rb") as handle:
            await send(
                {
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"application/octet-stream")],
                }
            )
            await send({"type": "http.response.body", "body": b"head:", "more_body": True})
            await send(
                {
                    "type": "http.response.zerocopysend",
                    "file": handle,
                    "offset": 0,
                    "count": 10,
                    "more_body": True,
                }
            )
            await send({"type": "http.response.body", "body": b":tail"})

        return

    await send({"type": "http.response.start", "status": 404, "headers": []})
    await send({"type": "http.response.body", "body": b"unknown test path\n"})


async def starlette_app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    """Starlette's FileResponse for the file the path names."""

    if scope["type"] == "lifespan":
        await asgi_app(scope, receive, send)
        return

    try:
        from starlette.responses import FileResponse
    except ImportError:
        await send({"type": "http.response.start", "status": 501, "headers": []})
        await send({"type": "http.response.body", "body": b"starlette is not installed\n"})
        return

    name = scope["path"].removeprefix(scope["root_path"]).strip("/")
    response = FileResponse(str(FILES_DIR / name), media_type="application/octet-stream")
    await response(scope, receive, send)
