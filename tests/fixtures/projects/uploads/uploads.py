"""Upload offloading (section 4.11): what applications see of the files
the edge stores. `wsgi_app` answers by the last segment of the path:
`describe` reports the uploads, the body and its length as JSON, `get`
looks one up by `?ref=`, `save` saves each into `?dest=` (trying again
without and then with overwrite), and `path` reads each through
`upload.path`. A precheck is recorded in the PRECHECKS file, with the
request's X-Tag header, and refused when X-Allow is no. `asgi_app` is the
ASGI side of `describe`.
"""

from __future__ import annotations

import json
import os
from collections.abc import Awaitable, Callable, Iterable
from typing import Any
from urllib.parse import parse_qs

from muru import uploads

PRECHECKS = os.environ.get("PRECHECKS", "")


def describe(upload: uploads.Upload) -> dict[str, Any]:
    with upload.open() as handle:
        content = handle.read()
        handle.seek(2)
        moved = handle.read(3)

    return {
        "field": upload.field,
        "name": upload.name,
        "content_type": upload.content_type,
        "size": upload.size,
        "sha256": upload.sha256,
        "digests": upload.digests,
        "ref": upload.ref,
        "content": content.decode("latin-1"),
        "moved": moved.decode("latin-1"),
    }


def record_precheck(tag: str) -> None:
    with open(PRECHECKS, "a") as log:
        log.write(f"{tag}\n")


def reply(start_response: Callable[..., Any], status: str, payload: Any) -> Iterable[bytes]:
    body = json.dumps(payload).encode()
    start_response(
        status, [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
    )
    return [body]


def wsgi_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    name = environ["PATH_INFO"].rsplit("/", 1)[-1]
    query = parse_qs(environ.get("QUERY_STRING", ""))

    if uploads.is_precheck(environ):
        record_precheck(environ.get("HTTP_X_TAG", ""))

        if environ.get("HTTP_X_ALLOW") == "no":
            return reply(start_response, "403 Forbidden", {"refused": True})

        return reply(start_response, "200 OK", {"allowed": True})

    length = int(environ.get("CONTENT_LENGTH") or 0)
    body = environ["wsgi.input"].read(length) if length else b""
    found = uploads.files(environ)

    if name == "get":
        upload = uploads.get(environ, query["ref"][0])
        return reply(start_response, "200 OK", {"found": upload and describe(upload)})

    if name == "save":
        saved = []

        for upload in found:
            target = os.path.join(query["dest"][0], f"{upload.ref}.bin")
            path = upload.save(target)

            try:
                upload.save(target)
                again = "saved twice"
            except FileExistsError:
                again = "exists"

            replaced = upload.save(target, overwrite=True)

            with open(path, "rb") as handle:
                content = handle.read()

            saved.append(
                {
                    "path": path,
                    "again": again,
                    "replaced": replaced,
                    "linked": os.stat(path).st_ino == os.stat(upload.path).st_ino,
                    "content": content.decode("latin-1"),
                }
            )

        return reply(start_response, "200 OK", {"saved": saved})

    if name == "path":
        contents = []

        for upload in found:
            with open(upload.path, "rb") as handle:
                contents.append(handle.read().decode("latin-1"))

        return reply(start_response, "200 OK", {"contents": contents})

    return reply(
        start_response,
        "200 OK",
        {
            "uploads": [describe(upload) for upload in found],
            "body": body.decode("latin-1"),
            "content_length": environ.get("CONTENT_LENGTH"),
            "content_type": environ.get("CONTENT_TYPE"),
        },
    )


async def asgi_app(
    scope: dict[str, Any],
    receive: Callable[[], Awaitable[dict[str, Any]]],
    send: Callable[[dict[str, Any]], Awaitable[None]],
) -> None:
    if scope["type"] == "lifespan":
        while True:
            message = await receive()

            if message["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            elif message["type"] == "lifespan.shutdown":
                await send({"type": "lifespan.shutdown.complete"})
                return

    body = b""
    more = True

    while more:
        message = await receive()
        body += message.get("body", b"")
        more = message.get("more_body", False)

    if uploads.is_precheck(scope):
        record_precheck("asgi")
        payload: Any = {"allowed": True}
    else:
        payload = {
            "uploads": [describe(upload) for upload in uploads.files(scope)],
            "body": body.decode("latin-1"),
        }

    encoded = json.dumps(payload).encode()
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"application/json")],
        }
    )
    await send({"type": "http.response.body", "body": encoded})
