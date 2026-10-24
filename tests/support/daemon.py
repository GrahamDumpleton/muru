"""A daemon under test (design specification sections 10 and 13.3).

The daemon runs in the foreground with state, runtime and log directories
of its own, and the tests drive it through muructl against its admin
socket. A second daemon over the same runtime directory and port re-adopts
what the first left running.
"""

from __future__ import annotations

import http.client
import socket
import ssl
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path

import pytest

from support import server as support

SHOP = support.ROOT / "tests" / "fixtures" / "projects" / "shop"
FACTORY = support.ROOT / "tests" / "fixtures" / "projects" / "factory"


@dataclass
class Daemon:
    """A daemon running in the foreground with directories of its own."""

    process: subprocess.Popen[bytes]
    port: int
    base: Path
    run_dir: Path
    log_path: Path
    listeners: list[tuple[int, bool]]

    def muructl(self, *arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        """Run a muructl command against this daemon."""

        muructl = support.muructl_path()
        assert muructl is not None
        completed = subprocess.run(
            [str(muructl), *arguments, "--socket", str(self.run_dir / "admin.sock")],
            capture_output=True,
            text=True,
            check=False,
            timeout=180,
        )

        if check and completed.returncode != 0:
            raise RuntimeError(
                f"muructl {' '.join(arguments)} failed:\n{completed.stdout}{completed.stderr}"
            )

        return completed

    def deploy(self, app: str, source: Path, *arguments: str) -> str:
        """Deploy and return the line muructl printed."""

        completed = self.muructl("deploy", app, str(source), "--python", sys.executable, *arguments)
        return completed.stdout.strip()

    def get(self, path: str = "/") -> support.Response:
        return self.request(path)

    def request(
        self,
        path: str = "/",
        host: str | None = None,
        port: int | None = None,
        cafile: Path | None = None,
        server_hostname: str | None = None,
        headers: dict[str, str] | None = None,
    ) -> support.Response:
        """One request, with a Host header and, given a CA file, over TLS.

        The TLS connection carries `server_hostname` (else `host`) as SNI
        and the certificate is checked against it. `headers` are sent as
        given, beside the Host header.
        """

        port = port or self.port
        headers = {**(headers or {}), **({"Host": host} if host else {})}
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)

        if cafile is not None:
            context = ssl.create_default_context(cafile=cafile)
            connection.sock = context.wrap_socket(
                socket.create_connection(("127.0.0.1", port), timeout=30),
                server_hostname=server_hostname or host or "localhost",
            )

        try:
            connection.request("GET", path, headers=headers)
            raw = connection.getresponse()
            return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
        finally:
            connection.close()

    @property
    def https_port(self) -> int | None:
        """The first TLS listener's port, when the daemon has one."""

        return next((port for port, tls in self.listeners if tls), None)

    def releases(self, app: str = "") -> list[dict[str, str]]:
        """The release table as muructl prints it, parsed by column."""

        lines = self.muructl("release", "list", *([app] if app else [])).stdout.splitlines()
        rows = []

        for line in lines[1:]:
            parts = line.split()
            if not parts or "@" not in parts[0]:
                continue
            rows.append(
                {
                    "release": parts[0],
                    "state": parts[1],
                    "serving": "yes" if "yes" in parts[2:3] else "",
                }
            )

        return rows

    def events(self) -> list[str]:
        return [line.split(None, 4)[3] for line in self.muructl("events").stdout.splitlines()]

    def log(self) -> str:
        return self.log_path.read_text(errors="replace")

    def stop(self, shutdown: bool = True) -> None:
        """Stop the daemon through its API, or kill it if that fails.

        With shutdown, every release and the edge stop with it, which is
        what a test's clean-up wants; without it the daemon detaches.
        """

        if self.process.poll() is None:
            arguments = ["daemon", "stop", "--shutdown"] if shutdown else ["daemon", "stop"]
            completed = self.muructl(*arguments, check=False)

            try:
                self.process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()

            if completed.returncode != 0:
                raise RuntimeError(
                    f"muructl daemon stop failed: {completed.stdout}{completed.stderr}"
                )


def start_daemon(
    base: Path,
    run_dir: Path | None = None,
    port: int | None = None,
    listeners: list[tuple[int, bool]] | None = None,
    config_extra: str = "",
    arguments: list[str] | None = None,
) -> Daemon:
    """Start a daemon in the foreground with directories under `base`.

    With `listeners`, pairs of port and whether it carries TLS, the daemon
    reads them from a configuration file written under `base`, and
    `config_extra` is appended to that file (the `[proxy]` table, say);
    otherwise it serves plain HTTP on `port`. `arguments` are passed to
    `muructl daemon run` as given. A second daemon over the
    same `run_dir` and listeners re-adopts what the first left running.
    """

    muructl = support.muructl_path()
    if muructl is None:
        support.unavailable("muructl is not built (cargo build -p muructl)")
    if not support.extension_available():
        support.unavailable("the muru extension is not built into this interpreter (just dev)")

    # The runtime directory holds the admin socket, whose path is limited
    # to about a hundred bytes, so it goes under /tmp rather than pytest's
    # deep temporary directory.

    run_dir = run_dir or Path(tempfile.mkdtemp(prefix="muru-run-", dir="/tmp"))

    if listeners is None and not config_extra:
        port = port or support.free_port()
        listeners = [(port, False)]
        where = ["--listen", f"127.0.0.1:{port}"]
    else:
        listeners = listeners or [(port or support.free_port(), False)]
        port = listeners[0][0]
        config = base / "muru.toml"
        config.write_text(
            "".join(
                f"[[listener]]\naddress = '127.0.0.1:{port}'\n" + ("tls = true\n" if tls else "")
                for port, tls in listeners
            )
            + config_extra
        )
        where = ["--config", str(config)]

    log_path = base / "daemon.out"

    with log_path.open("ab") as handle:
        process = subprocess.Popen(
            [
                str(muructl),
                "daemon",
                "run",
                "--state-dir",
                str(base / "state"),
                "--run-dir",
                str(run_dir),
                "--log-dir",
                str(base / "logs"),
                *where,
                *(arguments or []),
            ],
            stdout=handle,
            stderr=subprocess.STDOUT,
        )

    daemon = Daemon(process, port, base, run_dir, log_path, listeners)

    deadline = time.monotonic() + 30

    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"the daemon exited early:\n{daemon.log()}")

        if (run_dir / "admin.sock").exists() and daemon.muructl(
            "status", check=False
        ).returncode == 0:
            return daemon

        time.sleep(0.1)

    raise RuntimeError(f"the daemon did not answer:\n{daemon.log()}")


@pytest.fixture
def daemon(tmp_path: Path) -> Iterator[Daemon]:
    """A daemon that is shut down, releases and all, when the test ends."""

    instance = start_daemon(tmp_path)

    try:
        yield instance
    finally:
        instance.stop()
