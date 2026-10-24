"""ASGI applications under `muructl run` (design specification section 6.6).

The plain ASGI fixture runs on the worker's event loop: scope contents,
request bodies as http.request messages, streaming responses, lifespan
state, errors before and after the response starts, protocol misuse by the
application, disconnects and concurrency beyond any thread count.
"""

from __future__ import annotations

import http.client
import json
import socket
import threading
import time
from collections.abc import Callable, Iterator
from typing import Any

import pytest
from support import server as support


@pytest.fixture(scope="module")
def server() -> Iterator[support.Server]:
    instance = support.start("plain_asgi:application", workers=1, threads=None)
    try:
        yield instance
    finally:
        instance.stop()


def details(server: support.Server, body: bytes | None = None, **headers: str) -> dict[str, Any]:
    """Fetch /info and return what the application saw."""

    method = "POST" if body else "GET"
    response = server.request(method, "/info", body=body, headers=headers)
    assert response.status == 200, response.body
    return json.loads(response.body)


def in_threads(count: int, work: Callable[[int], None]) -> None:
    """Run `work` in `count` threads at once and wait for them all."""

    threads = [threading.Thread(target=work, args=(index,)) for index in range(count)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()


def read_until_failure(server: support.Server, path: str) -> None:
    """Read a response that the server cuts short, raising as http.client does."""

    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=30)
    try:
        connection.request("GET", path)
        raw = connection.getresponse()
        assert raw.status == 200
        raw.read()
    finally:
        connection.close()


def test_served_as_asgi(server: support.Server) -> None:
    response = server.get("/")

    assert response.status == 200
    assert response.body == b"Hello, async world!\n"
    assert "plain_asgi:application is served as ASGI" in server.log()
    assert "with 1 workers of concurrency 1000" in server.log()


def test_scope_contents(server: support.Server) -> None:
    data = details(server, **{"X-Custom": "yes", "Host": "example.test"})
    scope = data["scope"]

    assert scope["type"] == "http"
    assert scope["asgi"] == {"version": "3.0", "spec_version": "2.4"}
    assert scope["http_version"] == "1.1"
    assert scope["method"] == "GET"
    assert scope["scheme"] == "http"
    assert scope["path"] == "/info"
    assert scope["raw_path"] == "/info"
    assert scope["query_string"] == ""
    assert scope["root_path"] == ""
    assert ["x-custom", "yes"] in scope["headers"]
    assert ["host", "example.test"] in scope["headers"]
    assert scope["client"][0] == "127.0.0.1"
    assert scope["server"] == ["127.0.0.1", server.port]

    assert data["thread"] == "muru-loop"
    assert data["extensions"]["muru"]["request_id"]
    assert data["extensions"]["muru"]["max_body_size"] == 100 * 1024 * 1024


def test_path_and_query_decoding(server: support.Server) -> None:
    data = json.loads(server.get("/info?x=1&y=caf%C3%A9").body)
    assert data["scope"]["path"] == "/info"
    assert data["scope"]["query_string"] == "x=1&y=caf%C3%A9"

    data = json.loads(server.get("/info%3Fnot-a-query").body)
    assert data["scope"]["path"] == "/info?not-a-query"
    assert data["scope"]["raw_path"] == "/info%3Fnot-a-query"


def test_lifespan_state_is_copied_into_each_scope(server: support.Server) -> None:
    first = json.loads(server.get("/state").body)
    second = json.loads(server.get("/state").body)

    assert first["started_in"] == second["started_in"] == details(server)["pid"]

    # Each request sees its own copy of the state the lifespan filled.

    assert first["counter"] == 1
    assert second["counter"] == 1


def test_request_body_arrives_as_messages(server: support.Server) -> None:
    body = b"x" * (256 * 1024)
    response = server.post("/echo", body)
    assert response.status == 200
    assert response.body == body

    data = details(server, body=b"small")
    assert data["body_length"] == 5
    assert data["body_messages"] >= 1


def test_chunked_request_body_is_buffered_with_a_length(server: support.Server) -> None:
    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=30)
    try:
        connection.putrequest("POST", "/info")
        connection.putheader("Transfer-Encoding", "chunked")
        connection.endheaders()
        for chunk in (b"abc", b"defgh"):
            connection.send(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
        connection.send(b"0\r\n\r\n")
        data = json.loads(connection.getresponse().read())
    finally:
        connection.close()

    assert data["body_length"] == 8
    assert ["content-length", "8"] in data["scope"]["headers"]


def test_streaming_response(server: support.Server) -> None:
    response = server.get("/stream?count=3")

    assert response.status == 200
    assert response.body == b"chunk 0\nchunk 1\nchunk 2\n"
    assert response.header("transfer-encoding") == "chunked"


def test_large_response(server: support.Server) -> None:
    response = server.get("/large?size=3000000")

    assert response.status == 200
    assert len(response.body) == 3_000_000


def test_sync_code_runs_on_the_sync_pool(server: support.Server) -> None:
    assert server.get("/thread").body.startswith(b"muru-sync")


def test_each_request_has_its_own_context(server: support.Server) -> None:
    results: list[bytes] = []
    in_threads(8, lambda index: results.append(server.get(f"/context?mark=m{index}").body))

    assert sorted(results) == sorted(f"m{index}".encode() for index in range(8))


def test_concurrency_is_not_bounded_by_threads(server: support.Server) -> None:
    # Many slow requests at once, on one worker with one loop: they overlap.

    count = 32
    results: list[int] = []
    started = time.monotonic()
    in_threads(count, lambda _: results.append(server.get("/sleep?seconds=0.5").status))
    elapsed = time.monotonic() - started

    assert results == [200] * count
    assert elapsed < 0.5 * count / 4, f"{count} requests took {elapsed:.1f}s"


def test_exception_before_response_is_a_500_with_traceback(server: support.Server) -> None:
    server.mark_log()
    response = server.get("/error", headers={"Accept": "text/plain"})

    assert response.status == 500
    assert b"deliberate failure before the response" in response.body
    assert server.wait_for_log("RuntimeError: deliberate failure before the response")


def test_exception_after_response_started_truncates_it(server: support.Server) -> None:
    server.mark_log()
    with pytest.raises((http.client.IncompleteRead, ConnectionError, http.client.HTTPException)):
        read_until_failure(server, "/error-after-start")

    assert server.wait_for_log("deliberate failure after the response started")


@pytest.mark.parametrize(
    ("path", "message"),
    [
        ("/body-before-start", "http.response.body sent before http.response.start"),
        ("/double-start", "http.response.start sent after the response started"),
        ("/bad-status", "invalid HTTP status 99"),
        ("/bad-header", "carriage return or line feed"),
    ],
)
def test_protocol_misuse_is_a_500(server: support.Server, path: str, message: str) -> None:
    server.mark_log()
    response = server.get(path, headers={"Accept": "text/plain"})

    assert response.status == 500, response.body
    assert server.wait_for_log(message)


def test_returning_without_a_response_is_a_500(server: support.Server) -> None:
    response = server.get("/no-response", headers={"Accept": "text/plain"})

    assert response.status == 500
    assert "returned without sending a response" in response.body.decode()


def test_body_after_the_response_ended_is_an_error_in_the_log(server: support.Server) -> None:
    server.mark_log()
    response = server.get("/body-after-end")

    assert response.status == 200
    assert response.body == b"done"
    assert server.wait_for_log("http.response.body sent after the response was completed")


def test_short_body_is_reported(server: support.Server) -> None:
    server.mark_log()
    with pytest.raises((http.client.IncompleteRead, ConnectionError)):
        read_until_failure(server, "/short")

    assert server.wait_for_log("ended after 5 of the 10 bytes")


def test_system_exit_does_not_end_the_worker(server: support.Server) -> None:
    pid_before = details(server)["pid"]
    response = server.get("/exit")

    assert response.status == 500
    assert details(server)["pid"] == pid_before


def test_client_disconnect_is_delivered(server: support.Server) -> None:
    server.mark_log()
    with socket.create_connection(("127.0.0.1", server.port), timeout=5) as sock:
        sock.sendall(b"GET /disconnect HTTP/1.1\r\nHost: x\r\n\r\n")
        time.sleep(0.2)

    assert server.wait_for_log("client disconnected")


def test_receive_after_response_gets_disconnect(server: support.Server) -> None:
    server.mark_log()
    response = server.get("/after-response")

    assert response.status == 200
    assert server.wait_for_log("after response: http.disconnect")


def test_lifespan_shutdown_runs_on_stop() -> None:
    instance = support.start("plain_asgi:application", workers=1, threads=None)
    try:
        assert instance.get("/").status == 200
    finally:
        instance.stop()

    assert "lifespan shutdown complete" in instance.log()
