"""Development defaults of `muructl run` (design specification section 13.1):
one process, wsgi.multiprocess False for a single-process run, a 100 MB body
limit shown as muru.max_body_size, and the HSTS warning."""

from __future__ import annotations

import json
import re

from support import server as support


def environ(server: support.Server) -> dict[str, object]:
    response = server.get("/info")
    assert response.status == 200
    data: dict[str, object] = json.loads(response.body)["environ"]
    return data


def test_single_process_run_reports_multiprocess_false() -> None:
    server = support.start("plain_wsgi:application", workers=None, threads=None)

    try:
        assert "with 1 workers of 4 threads" in server.log()
        data = environ(server)
        assert data["wsgi.multiprocess"] is False
        assert data["wsgi.multithread"] is True
        assert data["muru.max_body_size"] == 100 * 1024 * 1024
    finally:
        server.stop()


def test_two_processes_report_multiprocess_true() -> None:
    server = support.start("plain_wsgi:application", workers=2, threads=1)

    try:
        data = environ(server)
        assert data["wsgi.multiprocess"] is True
        assert data["wsgi.multithread"] is False
    finally:
        server.stop()


def test_body_limit_is_a_setting() -> None:
    server = support.start("plain_wsgi:application", workers=1, args=["--max-body-size", "1MB"])

    try:
        assert environ(server)["muru.max_body_size"] == 1024 * 1024
        response = server.raw(
            b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: %d\r\nConnection: close\r\n\r\n"
            % (2 * 1024 * 1024)
        )
        assert response.startswith(b"HTTP/1.1 413"), response
        assert b"1048576 bytes" in response
    finally:
        server.stop()


def test_hsts_from_the_application_is_warned_about_once() -> None:
    server = support.start("plain_wsgi:application", workers=1)

    try:
        for _ in range(2):
            response = server.get("/hsts")
            assert response.status == 200
            assert response.header("Strict-Transport-Security") == "max-age=31536000"

        assert server.wait_for_log("Strict-Transport-Security on /hsts")
        assert len(re.findall("Strict-Transport-Security on /hsts", server.log())) == 1
    finally:
        server.stop()
