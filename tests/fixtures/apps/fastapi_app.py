"""A small FastAPI application, for running FastAPI's behaviour through Muru."""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import os
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Annotated, Any

from fastapi import FastAPI, File, Form, HTTPException, Request, UploadFile, WebSocket
from fastapi.responses import (
    FileResponse,
    JSONResponse,
    PlainTextResponse,
    RedirectResponse,
    Response,
    StreamingResponse,
)

HERE = Path(__file__).resolve().parent


@contextlib.asynccontextmanager
async def lifespan(app: FastAPI) -> AsyncIterator[dict[str, Any]]:
    """Fill the lifespan state with the process that started."""

    yield {"started_in": os.getpid()}


application = FastAPI(lifespan=lifespan)


@application.get("/", response_class=PlainTextResponse)
async def index() -> str:
    return "Hello from FastAPI\n"


@application.get("/json")
async def as_json(request: Request) -> dict[str, Any]:
    return {"pid": os.getpid(), "path": request.url.path, "args": dict(request.query_params)}


@application.post("/echo")
async def echo(request: Request) -> Response:
    body = await request.body()
    content_type = request.headers.get("content-type", "application/octet-stream")
    return Response(body, media_type=content_type)


@application.post("/form")
async def form(
    name: Annotated[str, Form()],
    kind: Annotated[str, Form()],
    file: Annotated[UploadFile, File()],
) -> dict[str, Any]:
    return {"fields": {"name": name, "kind": kind}, "files": [file.filename and "file"]}


@application.post("/upload")
async def upload(file: Annotated[UploadFile, File()]) -> dict[str, Any]:
    data = await file.read()
    return {"name": file.filename, "size": len(data), "sha": hashlib.sha256(data).hexdigest()}


@application.get("/cookie")
async def cookie(request: Request) -> Response:
    response = JSONResponse({"seen": request.cookies.get("seen")})
    response.set_cookie("seen", "yes", httponly=True)
    return response


@application.get("/redirect")
async def redirect_home() -> Response:
    return RedirectResponse("/", status_code=302)


@application.get("/stream")
async def stream() -> Response:
    async def lines() -> AsyncIterator[bytes]:
        for index in range(5):
            yield f"line {index}\n".encode()
            await asyncio.sleep(0.02)

    return StreamingResponse(lines(), media_type="text/plain")


@application.get("/error")
async def error() -> None:
    raise RuntimeError("fastapi-boom")


@application.get("/forbidden")
async def forbidden() -> None:
    raise HTTPException(status_code=403)


@application.get("/sendfile")
async def sendfile() -> Response:
    return FileResponse(HERE / "fastapi_app.py", media_type="text/x-python")


@application.get("/state")
async def state(request: Request) -> dict[str, Any]:
    return dict(request.state._state)


@application.get("/sync-thread")
def sync_thread() -> dict[str, str]:
    """A plain function route, which FastAPI runs on a thread."""

    import threading

    return {"thread": threading.current_thread().name}


@application.websocket("/ws")
async def websocket_echo(socket: WebSocket) -> None:
    """Echo messages until the client closes."""

    offered = socket.scope["subprotocols"]
    await socket.accept(subprotocol=offered[0] if offered else None)
    while True:
        message = await socket.receive()
        if message["type"] == "websocket.disconnect":
            print(f"fastapi socket closed with {message['code']}", flush=True)
            return
        if message.get("text") is not None:
            await socket.send_text(message["text"])
        else:
            await socket.send_bytes(message["bytes"])
