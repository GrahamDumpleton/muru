"""A small Flask application, for running Flask's behaviour through Muru."""

from __future__ import annotations

import json
import os
import time
from collections.abc import Iterator

from flask import Flask, Response, abort, jsonify, make_response, request, stream_with_context

app = Flask(__name__)
app.config["MAX_CONTENT_LENGTH"] = 4 * 1024 * 1024


@app.get("/")
def index() -> Response:
    return Response("Hello from Flask\n", mimetype="text/plain")


@app.get("/json")
def as_json() -> Response:
    return jsonify(pid=os.getpid(), path=request.path, args=request.args.to_dict())


@app.post("/echo")
def echo() -> Response:
    response = make_response(request.get_data())
    response.content_type = request.content_type or "application/octet-stream"
    return response


@app.post("/form")
def form() -> Response:
    return jsonify(fields=request.form.to_dict(), files=sorted(request.files))


@app.post("/upload")
def upload() -> Response:
    upload = request.files["file"]
    data = upload.read()
    return jsonify(
        name=upload.filename, size=len(data), sha=__import__("hashlib").sha256(data).hexdigest()
    )


@app.get("/cookie")
def cookie() -> Response:
    response = make_response(json.dumps({"seen": request.cookies.get("seen")}))
    response.set_cookie("seen", "yes", httponly=True)
    return response


@app.get("/redirect")
def redirect_home() -> Response:
    from flask import redirect, url_for

    return redirect(url_for("index"))


@app.get("/stream")
def stream() -> Response:
    def generate() -> Iterator[bytes]:
        for index in range(5):
            yield f"line {index}\n".encode()
            time.sleep(0.02)

    return Response(stream_with_context(generate()), mimetype="text/plain")


@app.get("/error")
def error() -> str:
    raise RuntimeError("flask-boom")


@app.get("/forbidden")
def forbidden() -> str:
    abort(403)


@app.get("/headers")
def headers() -> Response:
    return jsonify({name: value for name, value in request.headers if name.startswith("X-")})


@app.get("/sendfile")
def sendfile() -> Response:
    from flask import send_file

    return send_file(__file__, mimetype="text/x-python")


application = app
