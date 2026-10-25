"""The http scope (ASGI 3.0, HTTP and WebSocket protocol specification 2.4)."""

from __future__ import annotations

import json
import shutil
import subprocess

import pytest
from support.server import Server

CURL = shutil.which("curl")


def scope(server: Server, target: str = "/scope", **headers: str) -> dict:
    return json.loads(server.get(target, headers=headers).body)


def test_type_and_versions(server: Server) -> None:
    report = scope(server)

    assert report["type"] == "http"
    assert report["asgi"] == {"version": "3.0", "spec_version": "2.4"}
    assert report["http_version"] == "1.1"
    assert report["method"] == "GET"
    assert report["scheme"] == "http"


def test_path_is_decoded_and_raw_path_is_not(server: Server) -> None:
    report = scope(server, "/scope%2Fsub%20dir?q=a%20b&x=%C3%A9")

    assert report["path"] == "/scope/sub dir"
    assert report["raw_path"] == "/scope%2Fsub%20dir"
    assert report["query_string"] == "q=a%20b&x=%C3%A9"
    assert report["root_path"] == ""


def test_unicode_path(server: Server) -> None:
    report = scope(server, "/scope/caf%C3%A9")

    assert report["path"] == "/scope/café"


def test_headers_are_lower_case_byte_pairs_in_order(server: Server) -> None:
    report = scope(server, **{"X-First": "1", "X-Second": "2", "Content-Type": "text/plain"})
    names = [name for name, _ in report["headers"]]

    assert ["x-first", "1"] in report["headers"]
    assert ["x-second", "2"] in report["headers"]
    assert ["content-type", "text/plain"] in report["headers"]
    assert names == [name.lower() for name in names]
    assert names.index("x-first") < names.index("x-second")


def test_hop_by_hop_headers_are_absent(server: Server) -> None:
    report = scope(server, **{"Connection": "keep-alive", "Keep-Alive": "timeout=5"})
    names = [name for name, _ in report["headers"]]

    assert "connection" not in names
    assert "keep-alive" not in names


def test_client_server_and_state(server: Server) -> None:
    report = scope(server)

    assert report["client"][0] == "127.0.0.1"
    assert isinstance(report["client"][1], int)
    assert report["server"] == ["127.0.0.1", server.port]
    assert report["state"]["pid"]
    assert report["extensions"] == [
        "http.response.pathsend",
        "http.response.zerocopysend",
        "muru",
    ]
    assert report["muru"]["request_id"]
    assert report["muru"]["max_body_size"] > 0


@pytest.mark.skipif(CURL is None, reason="curl is not installed")
def test_http2_version(server: Server) -> None:
    output = subprocess.run(
        [str(CURL), "-s", "--http2-prior-knowledge", f"http://127.0.0.1:{server.port}/scope"],
        capture_output=True,
        check=True,
    ).stdout
    report = json.loads(output)

    assert report["http_version"] == "2"
    assert ["host", f"127.0.0.1:{server.port}"] in report["headers"]
