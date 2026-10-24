"""HTTP/2 over cleartext by prior knowledge, driven with curl."""

from __future__ import annotations

import hashlib
import shutil
import subprocess
from pathlib import Path

import pytest
from support.server import Server

CURL = shutil.which("curl")


def curl_has_http2() -> bool:
    if CURL is None:
        return False
    output = subprocess.run([CURL, "--version"], capture_output=True, text=True, check=False)
    return "HTTP2" in output.stdout


pytestmark = pytest.mark.skipif(not curl_has_http2(), reason="curl with HTTP/2 support is needed")


def curl(server: Server, *args: str, data: bytes | None = None) -> tuple[list[str], bytes]:
    """Run curl with prior-knowledge HTTP/2 and return the status lines and body."""

    assert CURL is not None
    command = [CURL, "-sS", "--http2-prior-knowledge", "-D", "-", *args]
    completed = subprocess.run(command, input=data, capture_output=True, check=False, timeout=30)
    assert completed.returncode == 0, completed.stderr.decode()
    head, _, body = completed.stdout.partition(b"\r\n\r\n")
    status_lines = [
        line.strip() for line in head.decode().split("\r\n") if line.startswith("HTTP/")
    ]
    return status_lines, body


def url(server: Server, path: str) -> str:
    return f"http://127.0.0.1:{server.port}{path}"


def test_requests_are_served_as_http2(server: Server) -> None:
    statuses, body = curl(server, url(server, "/environ/get?key=SERVER_PROTOCOL"))
    assert statuses == ["HTTP/2 200"]
    assert body == b"value=HTTP/2.0;end"


def test_host_comes_from_the_authority(server: Server) -> None:
    _statuses, body = curl(server, url(server, "/environ/get?key=HTTP_HOST"))
    assert body == b"value=127.0.0.1:%d;end" % server.port


def test_request_body_reaches_the_application(server: Server) -> None:
    _statuses, body = curl(
        server, url(server, "/input/read-all"), "--data-binary", "@-", data=b"hello over h2"
    )
    assert body == b"data=hello over h2;end"


def test_large_bodies_in_both_directions(server: Server, tmp_path: Path) -> None:
    payload = bytes(range(256)) * 8192
    _statuses, body = curl(
        server,
        url(server, "/input/digest"),
        "--data-binary",
        "@-",
        "-H",
        "Expect:",
        data=payload,
    )
    assert body == f"len={len(payload)} sha256={hashlib.sha256(payload).hexdigest()};end".encode()
    _statuses, body = curl(server, url(server, "/response/large?size=3000000"))
    assert len(body) == 3000000
    assert body == bytes(range(256)) * (3000000 // 256) + bytes(range(3000000 % 256))


def test_streams_multiplex_on_one_connection(server: Server) -> None:
    # --parallel sends the requests at once; with HTTP/2 they share a
    # connection as streams, so far fewer connections than requests open.

    assert CURL is not None
    paths = ["/response/slow-chunks"] * 6 + ["/response/list"] * 6
    command = [
        CURL,
        "-sS",
        "--http2-prior-knowledge",
        "--parallel",
        "-w",
        "%{http_version} %{response_code} %{num_connects}\n",
    ]

    for path in paths:
        # One --output per URL, or curl prints the later bodies to stdout.

        command.extend(["-o", "/dev/null", url(server, path)])

    completed = subprocess.run(command, capture_output=True, text=True, check=False, timeout=30)
    assert completed.returncode == 0, completed.stderr
    results = [line.split() for line in completed.stdout.splitlines()]
    assert [result[:2] for result in results] == [["2", "200"]] * 12
    assert sum(int(result[2]) for result in results) < 6, completed.stdout


def test_reason_phrase_is_dropped(server: Server) -> None:
    # HTTP/2 has no reason phrase; the status still arrives.

    statuses, body = curl(server, url(server, "/start-response/status-reason-phrase"))
    assert statuses == ["HTTP/2 299"]
    assert body.endswith(b";end")


def test_error_from_the_application_is_a_500(server: Server) -> None:
    statuses, _body = curl(server, url(server, "/response/raise-first"))
    assert statuses == ["HTTP/2 500"]


def test_http1_still_works_on_the_same_listener(server: Server) -> None:
    assert server.get("/response/list").status == 200
