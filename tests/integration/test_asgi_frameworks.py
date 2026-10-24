"""Starlette, FastAPI and Django's ASGI handler through muructl run.

Each framework's own behaviour over HTTP, as the WSGI framework tests
check for Flask and Django, plus WebSockets and the lifespan state for the
two frameworks that have them.
"""

from __future__ import annotations

import hashlib
import http.client
import json
import os
from collections.abc import Iterator

import pytest
from support import server as support
from test_frameworks import multipart
from websockets.sync.client import connect

STARLETTE = ("starlette_app:application", "Hello from Starlette\n")
FASTAPI = ("fastapi_app:application", "Hello from FastAPI\n")
DJANGO = ("django_site.asgi:application", "Hello from Django\n")


@pytest.fixture(
    scope="module", params=[STARLETTE, FASTAPI, DJANGO], ids=["starlette", "fastapi", "django"]
)
def framework(request: pytest.FixtureRequest) -> Iterator[tuple[support.Server, str]]:
    app, greeting = request.param
    instance = support.start(app, workers=2, threads=None)

    try:
        yield instance, greeting
    finally:
        instance.stop()


def has_websockets(greeting: str) -> bool:
    """Django's ASGI handler serves HTTP only; the other two take WebSockets."""

    return "Django" not in greeting


def test_served_as_asgi(framework: tuple[support.Server, str]) -> None:
    server, greeting = framework
    response = server.get("/")

    assert response.status == 200
    assert response.body.decode() == greeting
    assert "is served as ASGI" in server.log()
    assert "preload:" not in server.log()


def test_json_and_query_string(framework: tuple[support.Server, str]) -> None:
    server, _ = framework
    body = json.loads(server.get("/json?a=1&b=two").body)

    assert body["path"] == "/json"
    assert body["args"] == {"a": "1", "b": "two"}
    assert body["pid"] in server.worker_pids().values()


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
    path = "/session" if "Django" in greeting else "/cookie"
    first = server.get(path)
    cookie = first.header("Set-Cookie")
    assert cookie

    second = server.get(path, headers={"Cookie": cookie.split(";", 1)[0]})

    if "Django" in greeting:
        assert json.loads(first.body) == {"count": 1}
        assert json.loads(second.body) == {"count": 2}
    else:
        assert json.loads(first.body) == {"seen": None}
        assert json.loads(second.body) == {"seen": "yes"}


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

    if "Django" not in greeting:
        # Starlette's ServerErrorMiddleware re-raises after its own 500,
        # so the traceback reaches the worker's log.

        assert server.wait_for_log("boom")

    assert server.get("/").status == 200


def test_framework_error_pages_pass_through(framework: tuple[support.Server, str]) -> None:
    server, _ = framework

    assert server.get("/forbidden").status == 403
    assert server.get("/no-such-page").status == 404


def test_send_file(framework: tuple[support.Server, str]) -> None:
    server, greeting = framework
    response = server.get("/sendfile")
    source = {
        "Starlette": "starlette_app.py",
        "FastAPI": "fastapi_app.py",
        "Django": "django_site/views.py",
    }[greeting.split()[2]]

    assert response.status == 200
    assert response.body == (support.FIXTURES / source).read_bytes()


def test_lifespan_state(framework: tuple[support.Server, str]) -> None:
    server, greeting = framework
    if not has_websockets(greeting):
        pytest.skip("Django has no lifespan state")

    state = json.loads(server.get("/state").body)
    assert state["started_in"] in server.worker_pids().values()


def test_websocket_echo(framework: tuple[support.Server, str]) -> None:
    server, greeting = framework
    if not has_websockets(greeting):
        pytest.skip("Django's ASGI handler serves HTTP only")

    server.mark_log()

    with connect(
        f"ws://127.0.0.1:{server.port}/ws", subprotocols=["chat"], open_timeout=10
    ) as socket_:  # type: ignore[arg-type]
        assert socket_.subprotocol == "chat"
        socket_.send("hello")
        assert socket_.recv() == "hello"
        socket_.send(b"\x01\x02")
        assert socket_.recv() == b"\x01\x02"
        socket_.close(code=4321)

    assert server.wait_for_log("socket closed with 4321")


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


def test_fastapi_sync_route_runs_on_a_thread() -> None:
    instance = support.start("fastapi_app:application", workers=1, threads=None)

    try:
        body = json.loads(instance.get("/sync-thread").body)
    finally:
        instance.stop()

    assert body["thread"] != "muru-loop"
