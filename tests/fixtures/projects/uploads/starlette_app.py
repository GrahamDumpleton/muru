"""The Starlette recipe for offloaded uploads (section 4.11), as the
uploads guide gives it, behind an endpoint that reports the form and the
files.
"""

from __future__ import annotations

from starlette.applications import Starlette
from starlette.datastructures import Headers, UploadFile
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from muru import uploads


def offloaded_files(request: Request) -> dict[str, UploadFile]:
    return {
        upload.field: UploadFile(
            upload.open(),
            size=upload.size,
            filename=upload.name,
            headers=Headers({"content-type": upload.content_type}),
        )
        for upload in uploads.files(request.scope)
        if upload.field is not None
    }


async def describe(request: Request) -> JSONResponse:
    form = await request.form()
    files = offloaded_files(request)
    return JSONResponse(
        {
            "fields": {name: value for name, value in form.items() if isinstance(value, str)},
            "files": {
                name: [stored.filename, stored.content_type, (await stored.read()).decode()]
                for name, stored in files.items()
            },
        }
    )


app = Starlette(routes=[Route("/describe", describe, methods=["POST"])])
