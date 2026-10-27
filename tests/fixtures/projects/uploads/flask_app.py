"""The Flask recipe for offloaded uploads (section 4.11), as the uploads
guide gives it, behind a view that reports the form and the files.
"""

from __future__ import annotations

from typing import Any

from flask import Flask, Request, jsonify, request
from werkzeug.datastructures import FileStorage, MultiDict

from muru import uploads

app = Flask(__name__)


def offloaded_files(request: Request) -> Any:
    found = uploads.files(request.environ)

    if not found:
        return request.files

    return MultiDict(
        (upload.field, FileStorage(upload.open(), upload.name, upload.field, upload.content_type))
        for upload in found
    )


@app.post("/describe")
def describe() -> Any:
    files = offloaded_files(request)
    return jsonify(
        fields=request.form.to_dict(),
        files={
            name: [stored.filename, stored.content_type, stored.read().decode()]
            for name, stored in files.items()
        },
    )
