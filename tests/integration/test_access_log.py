"""The access log in development mode (design specification sections 11.2 and 13.1)."""

from __future__ import annotations

import json
import re
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support

LINE = re.compile(
    r"^(?P<time>\d\d:\d\d:\d\d\.\d{3}) (?P<status>\d{3}) (?P<method>[A-Z]+) (?P<target>\S+) "
    r"(?P<bytes>\S+) (?P<total>\S+)(?P<rest>.*) id=(?P<id>[0-9A-Z]{26})$"
)


def access_lines(server: support.Server) -> list[re.Match[str]]:
    matches = []
    for line in server.log_since_mark().splitlines():
        match = LINE.match(line)
        if match:
            matches.append(match)
    return matches


@pytest.fixture
def default() -> Iterator[support.Server]:
    instance = support.start("wsgi_conformance:application", workers=1)
    try:
        yield instance
    finally:
        instance.stop()


def test_compact_lines_describe_each_request(default: support.Server) -> None:
    default.mark_log()
    response = default.get("/response/list")
    assert default.wait_for_log(f"id={response.header('X-Request-ID')}")
    lines = access_lines(default)
    assert len(lines) == 1
    line = lines[0]
    assert line["status"] == "200"
    assert line["method"] == "GET"
    assert line["target"] == "/response/list"
    assert line["bytes"] == "20B"
    assert line["id"] == response.header("X-Request-ID")
    assert "queue=" in line["rest"] and "app=" in line["rest"]
    assert "worker=0" in line["rest"]


def test_edge_errors_carry_their_reason(default: support.Server) -> None:
    default.mark_log()
    response = default.get("/response/raise-first")
    assert response.status == 500
    assert default.wait_for_log(f"id={response.header('X-Request-ID')}")
    line = access_lines(default)[-1]
    assert line["status"] == "500"
    assert "reason=app_error" in line["rest"]


def test_query_strings_are_kept(default: support.Server) -> None:
    default.mark_log()
    response = default.get("/environ/get?key=PATH_INFO")
    assert default.wait_for_log(f"id={response.header('X-Request-ID')}")
    assert access_lines(default)[-1]["target"] == "/environ/get?key=PATH_INFO"


def test_off_writes_nothing() -> None:
    server = support.start("wsgi_conformance:application", workers=1, args=["--access-log", "off"])
    try:
        server.mark_log()
        for _ in range(3):
            assert server.get("/response/list").status == 200
        assert server.get("/response/raise-first").status == 500
        assert not server.wait_for_log(" GET /response/", timeout=0.5)
    finally:
        server.stop()


def test_errors_mode_shows_failures_and_slow_requests() -> None:
    server = support.start(
        "wsgi_conformance:application",
        workers=1,
        args=["--access-log", "errors", "--slow", "100ms"],
    )
    try:
        server.mark_log()
        ok = server.get("/response/list")
        failed = server.get("/response/raise-first")
        slow = server.get("/response/slow-chunks")
        assert server.wait_for_log(f"id={slow.header('X-Request-ID')}")
        ids = {line["id"] for line in access_lines(server)}
        assert ok.header("X-Request-ID") not in ids
        assert failed.header("X-Request-ID") in ids
        assert slow.header("X-Request-ID") in ids
    finally:
        server.stop()


def test_excluded_paths_are_hidden() -> None:
    server = support.start(
        "wsgi_conformance:application",
        workers=1,
        args=["--access-log-exclude", "/environ/*", "--access-log-exclude", "/method/?cho"],
    )
    try:
        server.mark_log()
        hidden = server.get("/environ/get?key=PATH_INFO")
        hidden_too = server.get("/method/echo")
        shown = server.get("/response/list")
        assert server.wait_for_log(f"id={shown.header('X-Request-ID')}")
        ids = {line["id"] for line in access_lines(server)}
        assert hidden.header("X-Request-ID") not in ids
        assert hidden_too.header("X-Request-ID") not in ids
        assert shown.header("X-Request-ID") in ids
    finally:
        server.stop()


def test_file_destination_writes_json_lines(tmp_path: Path) -> None:
    log = tmp_path / "access.log"
    server = support.start(
        "wsgi_conformance:application", workers=1, args=["--access-log", str(log)]
    )
    try:
        server.mark_log()
        response = server.post("/input/read-all", b"hello")
        assert not server.wait_for_log("POST /input/read-all", timeout=0.3)
        records = [json.loads(line) for line in log.read_text().splitlines()]
        assert len(records) == 1
        record = records[0]
        assert record["request_id"] == response.header("X-Request-ID")
        assert record["method"] == "POST"
        assert record["target"] == "/input/read-all"
        assert record["status"] == 200
        assert record["bytes"] == len(response.body)
        assert record["worker"] == 0
        assert record["client"] == "127.0.0.1"
        assert record["version"] == "HTTP/1.1"
        assert record["total_ms"] >= record["app_ms"] >= 0
        assert re.match(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$", record["time"])
    finally:
        server.stop()
