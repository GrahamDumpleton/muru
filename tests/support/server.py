"""Start `muructl run` for a fixture application and talk to it over HTTP."""

from __future__ import annotations

import http.client
import os
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import time
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures" / "apps"


def muructl_path() -> Path | None:
    """Return the muructl binary to test, or None when it is not built."""

    candidate = Path(os.environ.get("MURUCTL", ROOT / "target" / "debug" / "muructl"))
    return candidate if candidate.is_file() else None


def extension_available() -> bool:
    """Whether the native extension is importable in this interpreter."""

    try:
        from muru import _native  # noqa: F401
    except ImportError:
        return False

    return True


def unavailable(reason: str) -> None:
    """Skip the test for a missing binary or extension, or fail it under CI.

    CI builds both before the suite runs, so there a skip would hide a
    broken build (as when the extension was built for the wrong ABI).
    """

    if os.environ.get("CI"):
        pytest.fail(f"{reason}; CI must not skip the server tests")
    pytest.skip(reason)


def free_port() -> int:
    """Pick a port that is free right now."""

    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


@dataclass
class Response:
    """One HTTP response, fully read."""

    status: int
    reason: str
    headers: list[tuple[str, str]]
    body: bytes

    def header(self, name: str) -> str | None:
        """Return the value of a header, with repeats joined by a comma."""

        values = self.header_values(name)
        return ", ".join(values) if values else None

    def header_values(self, name: str) -> list[str]:
        """Return every value of a header, in order."""

        return [value for key, value in self.headers if key.lower() == name.lower()]


@dataclass
class Server:
    """A running `muructl run` serving one fixture application."""

    port: int
    process: subprocess.Popen[bytes]
    log_path: Path
    workers: int
    threads: int
    https_port: int | None = None
    _log_offset: int = field(default=0, repr=False)

    def request(
        self,
        method: str,
        path: str,
        body: bytes | None = None,
        headers: dict[str, str] | None = None,
    ) -> Response:
        """Make one request on a fresh connection and read the whole response."""

        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)

        try:
            connection.request(method, path, body=body, headers=headers or {})
            raw = connection.getresponse()
            return Response(raw.status, raw.reason, raw.getheaders(), raw.read())
        finally:
            connection.close()

    def request_tls(
        self,
        method: str,
        path: str,
        cafile: Path,
        server_hostname: str = "localhost",
        body: bytes | None = None,
        headers: dict[str, str] | None = None,
    ) -> Response:
        """Make one request over HTTPS, trusting `cafile`, and read the whole response.

        The connection goes to 127.0.0.1 while the certificate is checked
        against `server_hostname`, which is also what the SNI carries.
        """

        assert self.https_port is not None, "the server was started without --https"
        context = ssl.create_default_context(cafile=cafile)
        connection = http.client.HTTPConnection("127.0.0.1", self.https_port, timeout=30)
        connection.sock = context.wrap_socket(
            socket.create_connection(("127.0.0.1", self.https_port), timeout=30),
            server_hostname=server_hostname,
        )

        try:
            connection.request(method, path, body=body, headers=headers or {})
            raw = connection.getresponse()
            return Response(raw.status, raw.reason, raw.getheaders(), raw.read())
        finally:
            connection.close()

    def get(self, path: str, headers: dict[str, str] | None = None) -> Response:
        """GET a path."""

        return self.request("GET", path, headers=headers)

    def post(self, path: str, body: bytes, headers: dict[str, str] | None = None) -> Response:
        """POST a body to a path."""

        return self.request("POST", path, body=body, headers=headers)

    def raw(self, data: bytes, timeout: float = 10.0) -> bytes:
        """Send bytes on a new connection and return everything until it closes.

        Requests should carry `Connection: close`, or the read waits for the
        timeout on a kept-alive connection.
        """

        with socket.create_connection(("127.0.0.1", self.port), timeout=timeout) as sock:
            sock.sendall(data)
            chunks = []

            while True:
                try:
                    chunk = sock.recv(65536)
                except TimeoutError:
                    break

                if not chunk:
                    break
                chunks.append(chunk)

            return b"".join(chunks)

    def log(self) -> str:
        """Return everything the server has written to its log so far."""

        return self.log_path.read_text(errors="replace")

    def mark_log(self) -> None:
        """Remember the current end of the log for `log_since_mark`."""

        self._log_offset = self.log_path.stat().st_size

    def log_since_mark(self) -> str:
        """Return the log written since `mark_log`."""

        with self.log_path.open("rb") as handle:
            handle.seek(self._log_offset)
            return handle.read().decode(errors="replace")

    def wait_for_log(self, text: str, timeout: float = 5.0) -> bool:
        """Wait until the log written since `mark_log` contains text."""

        deadline = time.monotonic() + timeout

        while time.monotonic() < deadline:
            if text in self.log_since_mark():
                return True
            time.sleep(0.02)

        return False

    def worker_pids(self) -> dict[int, int]:
        """Return the current pid of each worker index, from the host's log."""

        pids: dict[int, int] = {}

        for line in self.log().splitlines():
            if "started as pid" in line:
                words = line.split()
                pids[int(words[words.index("worker") + 1])] = int(words[-1])

        return pids

    def host_pid(self) -> int:
        """Return the pid of the host process, from its log prefix."""

        for line in self.log().splitlines():
            if line.startswith("muru.host["):
                return int(line[len("muru.host[") : line.index("]")])

        raise AssertionError("the host has not logged anything")

    def stop(self) -> None:
        """Stop the server and its workers."""

        if self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)

            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()


def wait_for_port(port: int, process: subprocess.Popen[bytes], timeout: float = 60.0) -> None:
    """Wait until something accepts connections on the port."""

    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"muructl exited early with {process.returncode}")

        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)

    raise RuntimeError("muructl did not start listening in time")


def start(
    app: str | None = None,
    workers: int | None = 2,
    threads: int | None = 2,
    chdir: Path | None = FIXTURES,
    args: Sequence[str] = (),
    env: dict[str, str] | None = None,
    https: bool = False,
    host: str = "127.0.0.1",
) -> Server:
    """Start muructl for `module:callable`, skipping the test when it cannot run.

    `app` may also be a project directory, or None to run the project in
    `chdir`; `workers`, `threads` and `chdir` are left to muructl's own
    defaults when None. `args` are further `muructl run` options and `env`
    extra environment variables. With `https`, a free HTTPS port is chosen
    and passed as --https-port (the caller adds --https, --https-only or
    --tls-cert). `host` is the address to bind, which is loopback unless a
    test needs the server reachable from a container (with --allow-public
    in `args`). When muructl exits before its workers are ready, the
    RuntimeError raised carries its output.
    """

    muructl = muructl_path()
    if muructl is None:
        unavailable("muructl is not built (cargo build -p muructl)")
    if not extension_available():
        unavailable("the muru extension is not built into this interpreter (just dev)")

    port = free_port()
    https_port = free_port() if https else None
    https_args = ["--https-port", str(https_port)] if https_port is not None else []
    options: list[str] = []
    if workers is not None:
        options += ["--processes", str(workers)]
    if threads is not None:
        options += ["--threads", str(threads)]
    if chdir is not None:
        options += ["--chdir", str(chdir)]
    log = Path(tempfile.mkstemp(prefix="muru-test-", suffix=".log")[1])

    with log.open("wb") as handle:
        process = subprocess.Popen(
            [
                str(muructl),
                "run",
                *([app] if app is not None else []),
                "--bind",
                f"{host}:{port}",
                "--python",
                sys.executable,
                *options,
                *https_args,
                *args,
            ],
            stdout=handle,
            stderr=subprocess.STDOUT,
            env={**os.environ, **(env or {})},
        )

    server = Server(port, process, log, workers or 0, threads or 0, https_port)

    try:
        wait_for_port(port, process)

        if https_port is not None:
            wait_for_port(https_port, process)
    except RuntimeError as error:
        server.stop()
        raise RuntimeError(f"{error}\n{server.log()}") from error

    return server


def text(response: Response) -> str:
    """Return the body of a fixture `respond()` reply without its `;end` marker."""

    assert response.body.endswith(b";end"), response.body
    return response.body[:-4].decode("utf-8", "surrogateescape")
