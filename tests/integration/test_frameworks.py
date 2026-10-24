"""Flask and Django fixtures through muructl run: each framework's own behaviour over HTTP."""

from __future__ import annotations

import hashlib
import http.client
import json
import os
import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support

FLASK = ("flask_app:application", "Hello from Flask\n")
DJANGO = ("django_site.wsgi:application", "Hello from Django\n")


@pytest.fixture(scope="module", params=[FLASK, DJANGO], ids=["flask", "django"])
def framework(request: pytest.FixtureRequest) -> Iterator[tuple[support.Server, str]]:
    app, greeting = request.param
    instance = support.start(app, workers=2, threads=2)
    try:
        yield instance, greeting
    finally:
        instance.stop()


def multipart(fields: dict[str, str], files: dict[str, tuple[str, bytes]]) -> tuple[bytes, str]:
    boundary = uuid.uuid4().hex
    parts = []
    for name, value in fields.items():
        head = f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n'
        parts.append(f"{head}{value}\r\n".encode())
    for name, (filename, data) in files.items():
        parts.append(
            f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; '
            f'filename="{filename}"\r\nContent-Type: application/octet-stream\r\n\r\n'.encode()
            + data
            + b"\r\n"
        )
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


def test_index(framework: tuple[support.Server, str]) -> None:
    server, greeting = framework
    response = server.get("/")
    assert response.status == 200
    assert response.body.decode() == greeting
    assert (response.header("Content-Type") or "").startswith("text/plain")


def test_json_and_query_string(framework: tuple[support.Server, str]) -> None:
    server, _ = framework
    body = json.loads(server.get("/json?a=1&b=two").body)
    assert body["path"] == "/json"
    assert body["args"] == {"a": "1", "b": "two"}
    assert body["pid"] in server.worker_pids().values()


def test_preload_is_fork_safe(framework: tuple[support.Server, str]) -> None:
    server, _ = framework
    assert "preload:" not in server.log()
    assert "preloaded" in server.log()


def test_echo_body(framework: tuple[support.Server, str]) -> None:
    server, _ = framework
    payload = os.urandom(200_000)
    response = server.post("/echo", payload, headers={"Content-Type": "application/octet-stream"})
    assert response.status == 200
    assert response.body == payload


def test_form_and_upload(framework: tuple[support.Server, str]) -> None:
    server, _ = framework
    data = os.urandom(70_000)
    body, content_type = multipart({"name": "muru", "kind": "test"}, {"file": ("blob.bin", data)})
    form = json.loads(server.post("/form", body, headers={"Content-Type": content_type}).body)
    assert form == {"fields": {"name": "muru", "kind": "test"}, "files": ["file"]}
    upload = json.loads(server.post("/upload", body, headers={"Content-Type": content_type}).body)
    assert upload == {
        "name": "blob.bin",
        "size": len(data),
        "sha": hashlib.sha256(data).hexdigest(),
    }


def test_cookies_or_session(framework: tuple[support.Server, str]) -> None:
    server, greeting = framework
    path = "/cookie" if "Flask" in greeting else "/session"
    first = server.get(path)
    cookie = first.header("Set-Cookie")
    assert cookie
    second = server.get(path, headers={"Cookie": cookie.split(";", 1)[0]})
    if "Flask" in greeting:
        assert json.loads(first.body) == {"seen": None}
        assert json.loads(second.body) == {"seen": "yes"}
    else:
        assert json.loads(first.body) == {"count": 1}
        assert json.loads(second.body) == {"count": 2}


def test_redirect(framework: tuple[support.Server, str]) -> None:
    server, _ = framework
    response = server.get("/redirect")
    assert response.status == 302
    assert (response.header("Location") or "").endswith("/")


def test_streaming_response(framework: tuple[support.Server, str]) -> None:
    server, _ = framework
    response = server.get("/stream")
    assert response.status == 200
    assert response.body == b"".join(f"line {index}\n".encode() for index in range(5))


def test_application_error_is_a_500(framework: tuple[support.Server, str]) -> None:
    server, greeting = framework
    server.mark_log()
    response = server.get("/error")
    assert response.status == 500
    if "Flask" in greeting:
        # Flask logs the exception; Django's default logging only does so
        # with DEBUG on, and answers with its own 500 page either way.
        assert server.wait_for_log("flask-boom")
    assert server.get("/").status == 200


def test_framework_error_pages_pass_through(framework: tuple[support.Server, str]) -> None:
    server, _ = framework
    assert server.get("/forbidden").status == 403
    assert server.get("/no-such-page").status == 404


def test_send_file(framework: tuple[support.Server, str]) -> None:
    server, greeting = framework
    response = server.get("/sendfile")
    assert response.status == 200
    source = "flask_app.py" if "Flask" in greeting else "django_site/views.py"
    assert response.body == (support.FIXTURES / source).read_bytes()


def test_keep_alive_across_requests(framework: tuple[support.Server, str]) -> None:
    server, greeting = framework
    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=30)
    try:
        for _ in range(10):
            connection.request("GET", "/")
            response = connection.getresponse()
            assert response.status == 200
            assert response.read().decode() == greeting
    finally:
        connection.close()


def test_fixture_sources_exist() -> None:
    assert (Path(support.FIXTURES) / "django_site" / "wsgi.py").is_file()
