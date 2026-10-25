"""Plain HTTP servers that stand in for unmanaged upstreams (section 4.13).

Each server answers every path with a JSON report of what it received,
and a few paths do something else: `/slow` waits, `/status/NNN` answers
that status, `/redirect` asks the edge for an internal redirect,
`/health` answers 200 or 503 as the server's `healthy` flag says, and
`/auth` decides like an authentication handler.
"""

from __future__ import annotations

import json
import os
import socket
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer, ThreadingHTTPServer
from pathlib import Path

from support import server as support


class Upstream:
    """One upstream server, on a TCP port or a Unix socket."""

    def __init__(self, name: str, unix: bool = False) -> None:
        self.name = name
        self.healthy = True
        self.requests: list[dict[str, object]] = []
        self.probes = 0
        upstream = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_GET(self) -> None:
                self.answer()

            def do_POST(self) -> None:
                self.answer()

            def do_HEAD(self) -> None:
                self.answer()

            def do_PUT(self) -> None:
                self.answer()

            def answer(self) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                path = self.path.split("?")[0]
                name = path.rstrip("/").rsplit("/", 1)[-1]
                headers = {name.lower(): value for name, value in self.headers.items()}
                report: dict[str, object] = {
                    "upstream": upstream.name,
                    "method": self.command,
                    "path": self.path,
                    "headers": headers,
                    "body": body.decode("utf-8", "replace"),
                }

                if name == "health":
                    upstream.probes += 1
                    self.reply(200 if upstream.healthy else 503, b"", [])
                    return

                upstream.requests.append(report)

                if name == "slow":
                    time.sleep(5)
                    self.reply(200, b"late", [])
                    return

                if "/status/" in path:
                    self.reply(int(name), b"as asked", [])
                    return

                if name == "redirect":
                    self.reply(200, b"redirect", [("X-Muru-Redirect", "/internal-info")])
                    return

                if name == "auth":
                    if self.headers.get("Cookie") == "session=ok":
                        original = self.headers.get("X-Original-URI", "")
                        self.reply(200, b"", [("X-Auth-User", f"upstream:{original}")])
                    else:
                        self.reply(401, b"who\n", [("WWW-Authenticate", 'Bearer realm="up"')])

                    return

                encoded = json.dumps(report).encode()
                self.reply(
                    200,
                    encoded,
                    [
                        ("Content-Type", "application/json"),
                        ("X-Served-By", upstream.name),
                        ("X-Muru-Redirect", "/internal-info"),
                    ],
                )

            def reply(self, status: int, body: bytes, headers: list[tuple[str, str]]) -> None:
                self.send_response(status)

                for name, value in headers:
                    self.send_header(name, value)

                self.send_header("Content-Length", str(len(body)))
                self.end_headers()

                if self.command != "HEAD":
                    self.wfile.write(body)

        if unix:
            directory = Path(tempfile.mkdtemp(prefix="muru-up-", dir="/tmp"))
            self.path: Path | None = directory / "upstream.sock"

            class UnixServer(ThreadingHTTPServer):
                address_family = socket.AF_UNIX
                daemon_threads = True

                def server_bind(self) -> None:
                    # HTTPServer's own takes the address apart as host and
                    # port, which a socket path is not.

                    self.socket.bind(self.server_address)

            self.server: HTTPServer = UnixServer(str(self.path), Handler)  # type: ignore[arg-type]
            self.address = f"unix:{self.path}"
            self.port = 0
        else:
            self.path = None
            self.port = support.free_port()
            self.server = ThreadingHTTPServer(("127.0.0.1", self.port), Handler)
            self.server.daemon_threads = True
            self.address = f"127.0.0.1:{self.port}"

        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()

        if self.path is not None and self.path.exists():
            os.unlink(self.path)
