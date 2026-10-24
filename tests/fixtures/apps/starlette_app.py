"""A small Starlette application, for running Starlette's behaviour through Muru."""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import json
import os
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

from starlette.applications import Starlette
from starlette.exceptions import HTTPException
from starlette.requests import Request
from starlette.responses import (
    FileResponse,
    JSONResponse,
    PlainTextResponse,
    RedirectResponse,
    Response,
    StreamingResponse,
)
from starlette.routing import Route, WebSocketRoute
from starlette.websockets import WebSocket, WebSocketDisconnect

HERE = Path(__file__).resolve().parent


@contextlib.asynccontextmanager
async def lifespan(app: Starlette) -> AsyncIterator[dict[str, Any]]:
    """Fill the lifespan state with the process that started."""

    yield {"started_in": os.getpid()}


async def index(request: Request) -> Response:
    return PlainTextResponse("Hello from Starlette\n")


async def as_json(request: Request) -> Response:
    return JSONResponse(
        {"pid": os.getpid(), "path": request.url.path, "args": dict(request.query_params)}
    )


async def echo(request: Request) -> Response:
    body = await request.body()
    content_type = request.headers.get("content-type", "application/octet-stream")
    return Response(body, media_type=content_type)


async def form(request: Request) -> Response:
    async with request.form() as fields:
        files = sorted(name for name, value in fields.multi_items() if hasattr(value, "filename"))
        plain = {name: value for name, value in fields.multi_items() if isinstance(value, str)}

    return JSONResponse({"fields": plain, "files": files})


async def upload(request: Request) -> Response:
    async with request.form() as fields:
        received = fields["file"]
        data = await received.read()  # type: ignore[union-attr]
        name = received.filename  # type: ignore[union-attr]

    return JSONResponse({"name": name, "size": len(data), "sha": hashlib.sha256(data).hexdigest()})


async def cookie(request: Request) -> Response:
    response = JSONResponse({"seen": request.cookies.get("seen")})
    response.set_cookie("seen", "yes", httponly=True)
    return response


async def redirect_home(request: Request) -> Response:
    return RedirectResponse("/", status_code=302)


async def stream(request: Request) -> Response:
    async def lines() -> AsyncIterator[bytes]:
        for index in range(5):
            yield f"line {index}\n".encode()
            await asyncio.sleep(0.02)

    return StreamingResponse(lines(), media_type="text/plain")


async def error(request: Request) -> Response:
    raise RuntimeError("starlette-boom")


async def forbidden(request: Request) -> Response:
    raise HTTPException(status_code=403)


async def sendfile(request: Request) -> Response:
    return FileResponse(HERE / "starlette_app.py", media_type="text/x-python")


async def state(request: Request) -> Response:
    return JSONResponse(dict(request.state._state))


async def websocket_echo(socket: WebSocket) -> None:
    """Echo messages until the client closes."""

    await socket.accept(
        subprotocol=socket.scope["subprotocols"][0] if socket.scope["subprotocols"] else None
    )

    try:
        while True:
            message = await socket.receive()

            if message["type"] == "websocket.disconnect":
                print(f"starlette socket closed with {message['code']}", flush=True)
                return

            if message.get("text") is not None:
                await socket.send_text(message["text"])
            else:
                await socket.send_bytes(message["bytes"])
    except WebSocketDisconnect:
        return


application = Starlette(
    routes=[
        Route("/", index),
        Route("/json", as_json),
        Route("/echo", echo, methods=["POST"]),
        Route("/form", form, methods=["POST"]),
        Route("/upload", upload, methods=["POST"]),
        Route("/cookie", cookie),
        Route("/redirect", redirect_home),
        Route("/stream", stream),
        Route("/error", error),
        Route("/forbidden", forbidden),
        Route("/sendfile", sendfile),
        Route("/state", state),
        WebSocketRoute("/ws", websocket_echo),
    ],
    lifespan=lifespan,
)

if __name__ == "__main__":
    print(json.dumps({"routes": [route.path for route in application.routes]}))  # type: ignore[attr-defined]
