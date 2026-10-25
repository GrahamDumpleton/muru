"""An HTTP server that stands in for an existing deployment (section 9.9).

It binds where ``--bind`` says, ``unix:/path`` or ``host:port``, and
answers every path with JSON about the request and itself. A few paths
do something else: ``/healthz`` answers 503 until the file named by
``LEGACY_READY`` exists (200 when the variable is unset), ``/crash``
answers and then exits, ``/redirect`` asks the edge for an internal
redirect to ``/info``, and ``/slow`` waits two seconds.
"""

from __future__ import annotations

import json
import os
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: object) -> None:
        print(f"legacy: {format % args}", flush=True)

    def do_GET(self) -> None:
        self.answer()

    def do_POST(self) -> None:
        self.answer()

    def answer(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        path = self.path.split("?")[0]

        if path == "/healthz":
            ready_file = os.environ.get("LEGACY_READY")
            ready = ready_file is None or os.path.exists(ready_file)
            self.reply(200 if ready else 503, b"", [])
            return

        if path == "/slow":
            time.sleep(2)

        if path == "/redirect":
            self.reply(200, b"redirect", [("X-Muru-Redirect", "/info")])
            return

        report = {
            "server": "legacy",
            "pid": os.getpid(),
            "method": self.command,
            "path": self.path,
            "headers": {name.lower(): value for name, value in self.headers.items()},
            "body": body.decode("utf-8", "replace"),
            "cwd": os.getcwd(),
            "env": {
                name: os.environ.get(name)
                for name in ("MURU_HTTP_BIND", "PORT", "MURU_INSTANCE", "LEGACY_SETTING")
            },
        }
        self.reply(200, json.dumps(report).encode(), [("Content-Type", "application/json")])

        if path == "/crash":
            threading.Timer(0.1, lambda: os._exit(3)).start()

    def reply(self, status: int, body: bytes, headers: list[tuple[str, str]]) -> None:
        self.send_response(status)

        for name, value in headers:
            self.send_header(name, value)

        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class UnixServer(ThreadingHTTPServer):
    address_family = socket.AF_UNIX
    daemon_threads = True

    def server_bind(self) -> None:
        self.socket.bind(self.server_address)


def main() -> None:
    bind = sys.argv[sys.argv.index("--bind") + 1]

    if bind.startswith("unix:"):
        server: ThreadingHTTPServer = UnixServer(bind[5:], Handler)  # type: ignore[arg-type]
    else:
        host, port = bind.rsplit(":", 1)
        server = ThreadingHTTPServer((host, int(port)), Handler)
        server.daemon_threads = True

    print(f"legacy listening on {bind}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
