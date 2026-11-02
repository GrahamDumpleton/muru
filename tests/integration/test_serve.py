"""muructl serve as a plain process (section 13.9 of the specification).

The behaviour a container sees, the settings in layers, the admin port's
probes and metrics, one output stream, the run directory and the runtime
library found and never fetched, is tested here without a container
runtime; running as a container's process 1 is in test_containers.py.
"""

from __future__ import annotations

import http.client
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import Path

import pytest
from support import server as support

TIMED = r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z "


@dataclass
class Serve:
    """A running muructl serve, its output read as it arrives."""

    process: subprocess.Popen[str]
    port: int
    admin_port: int
    run_dir: Path
    stdout: list[str] = field(default_factory=list)
    stderr: list[str] = field(default_factory=list)

    def get(self, path: str, port: int | None = None) -> tuple[int, str]:
        connection = http.client.HTTPConnection("127.0.0.1", port or self.port, timeout=10)

        try:
            connection.request("GET", path)
            response = connection.getresponse()
            return response.status, response.read().decode()
        finally:
            connection.close()

    def admin(self, path: str) -> tuple[int, str]:
        return self.get(path, self.admin_port)

    def output(self) -> str:
        return "".join(self.stdout)

    def wait_for(self, text: str, timeout: float = 10.0) -> None:
        deadline = time.monotonic() + timeout

        while text not in self.output():
            assert time.monotonic() < deadline, (
                f"{text!r} not in:\n{self.output()}{''.join(self.stderr)}"
            )
            time.sleep(0.05)

    def stop(self) -> int:
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)

        try:
            return self.process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.process.kill()
            return self.process.wait()
        finally:
            if self.run_dir.parent == Path("/tmp"):
                shutil.rmtree(self.run_dir, ignore_errors=True)


def start(
    tmp_path: Path,
    *args: str,
    env: dict[str, str] | None = None,
    app: str | None = "plain_wsgi:application",
    wait: bool = True,
) -> Serve:
    muructl = support.muructl_path()
    if muructl is None:
        support.unavailable("muructl is not built (cargo build -p muructl)")
    if not support.extension_available():
        support.unavailable("the muru extension is not built into this interpreter (just dev)")

    port = support.free_port()
    admin_port = support.free_port()

    # A socket's path is limited to about a hundred bytes, which pytest's
    # temporary directories on macOS come close to.

    run_dir = Path(tempfile.mkdtemp(prefix="muru-serve-", dir="/tmp"))

    # The test's own flags stand in for these defaults.

    defaults = {
        "--chdir": str(support.FIXTURES),
        "--listen": f"127.0.0.1:{port}",
        "--admin-listen": f"127.0.0.1:{admin_port}",
        "--run-dir": str(run_dir),
        "--python": sys.executable,
        "--processes": "1",
    }
    options = [
        item for flag, value in defaults.items() if flag not in args for item in (flag, value)
    ]

    if "--run-dir" in args:
        run_dir = Path(args[args.index("--run-dir") + 1])

    process = subprocess.Popen(
        [str(muructl), "serve", *([app] if app is not None else []), *options, *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env={**os.environ, **(env or {})},
    )
    serve = Serve(process, port, admin_port, run_dir)

    def read(stream: Iterator[str], into: list[str]) -> None:
        for line in stream:
            into.append(line)

    assert process.stdout is not None and process.stderr is not None
    threading.Thread(target=read, args=(process.stdout, serve.stdout), daemon=True).start()
    threading.Thread(target=read, args=(process.stderr, serve.stderr), daemon=True).start()

    if wait:
        try:
            support.wait_for_port(port, process)
        except RuntimeError as error:
            serve.stop()
            raise RuntimeError(f"{error}\n{serve.output()}{''.join(serve.stderr)}") from error

    return serve


def test_serves_with_probes_and_metrics_on_the_admin_port(tmp_path: Path) -> None:
    serve = start(tmp_path)

    try:
        assert serve.get("/")[0] == 200
        assert serve.admin("/livez") == (200, "ok\n")
        assert serve.admin("/startupz") == (200, "ok\n")
        assert serve.admin("/readyz") == (200, "ok\n")
        status, metrics = serve.admin("/metrics")
        assert status == 200
        assert "muru_requests_total" in metrics

        # /status is opt-in, and the probes are not on the application's port.

        assert serve.admin("/status")[0] == 404
        assert serve.get("/readyz")[1] != "ok\n"
    finally:
        assert serve.stop() == 0


def test_sigterm_fails_readiness_and_keeps_accepting_for_the_delay(tmp_path: Path) -> None:
    serve = start(tmp_path, "--shutdown-delay", "2s", "--shutdown-budget", "6s")

    try:
        assert serve.admin("/readyz") == (200, "ok\n")
        signalled = time.monotonic()
        serve.process.send_signal(signal.SIGTERM)

        # Readiness fails at once, while the listener still serves, with
        # every connection closing after its response.

        deadline = time.monotonic() + 1

        while serve.admin("/readyz")[0] == 200:
            assert time.monotonic() < deadline, "readiness still passing after SIGTERM"
            time.sleep(0.05)

        connection = http.client.HTTPConnection("127.0.0.1", serve.port, timeout=10)

        try:
            connection.request("GET", "/")
            response = connection.getresponse()
            response.read()
            assert response.status == 200
            assert response.getheader("Connection") == "close"
        finally:
            connection.close()

        assert serve.process.wait(timeout=10) == 0
        assert 2 <= time.monotonic() - signalled < 6
    finally:
        serve.stop()


def test_everything_goes_to_standard_output_without_times(tmp_path: Path) -> None:
    import re

    serve = start(tmp_path)

    try:
        serve.get("/")
        serve.wait_for(" 200 GET / ")
    finally:
        assert serve.stop() == 0

    lines = serve.output().splitlines()
    assert serve.stderr == []
    assert any(line.startswith("muru.host[") for line in lines), lines
    assert any(line.startswith("muru: serving plain_wsgi:application") for line in lines), lines

    # The access line starts with the request's full UTC time, since
    # standard output is a pipe here; the other lines have none.

    access = next(line for line in lines if " 200 GET / " in line)
    assert re.match(TIMED, access), access
    assert not any(re.match(TIMED, line) for line in lines if line is not access)


def test_settings_come_from_the_environment(tmp_path: Path) -> None:
    serve = start(tmp_path, env={"MURU_SERVE_ADMIN_STATUS": "true", "MURU_SERVE_THREADS": "2"})

    try:
        status, body = serve.admin("/status")
        assert status == 200, body
        report = json.loads(body)
        assert report["state"] == "serving"
        assert report["settings"]["threads"] == 2
        assert report["groups"]["default"]["ready"] == 1
    finally:
        assert serve.stop() == 0


def test_print_config_names_where_each_setting_came_from(tmp_path: Path) -> None:
    config = tmp_path / "serve.toml"
    config.write_text('access_log = "errors"\nthreads = 3\n')
    serve = start(
        tmp_path,
        "--print-config",
        env={"MURU_SERVE_CONFIG": str(config), "MURU_SERVE_THREADS": "5"},
        wait=False,
    )
    assert serve.process.wait(timeout=30) == 0
    time.sleep(0.2)
    output = serve.output()
    assert f'access_log = "errors"    # {config}' in output, output
    assert 'threads = "5"    # environment' in output, output
    assert 'processes = "1"    # command line' in output, output


def test_json_lines_carry_their_kind_time_and_pod(tmp_path: Path) -> None:
    serve = start(
        tmp_path, "--log-format", "json", env={"POD_NAME": "shop-1", "POD_NAMESPACE": "prod"}
    )

    try:
        serve.get("/")
        serve.get("/error")
        serve.wait_for("RuntimeError: deliberate failure")
    finally:
        assert serve.stop() == 0

    records = [json.loads(line) for line in serve.output().splitlines()]
    kinds = {record["kind"] for record in records}
    assert {"muru", "app", "access"} <= kinds, kinds
    assert all(record["time"].endswith("Z") for record in records)
    assert all((record["pod"], record["namespace"]) == ("shop-1", "prod") for record in records)
    host = next(record for record in records if record["kind"] == "app")
    assert host["source"].startswith("muru.host["), host

    # The traceback is one record, from its first line to the exception's.

    traceback = next(record for record in records if "Traceback" in record.get("message", ""))
    assert traceback["source"] == "muru.worker[0]", traceback
    assert traceback["message"].startswith("Traceback (most recent call last):\n")
    assert traceback["message"].endswith("RuntimeError: deliberate failure")


def test_python_records_in_json_are_the_records_themselves(tmp_path: Path) -> None:
    serve = start(tmp_path, "--log-format", "json", app="logging_wsgi:application")

    try:
        serve.get("/fail")
        serve.wait_for('"level":"ERROR"')
    finally:
        assert serve.stop() == 0

    records = [json.loads(line) for line in serve.output().splitlines()]
    failed = next(record for record in records if record.get("message") == "failed for /fail")
    access = next(record for record in records if record["kind"] == "access")
    assert failed["kind"] == "app" and failed["logger"] == "demo", failed
    assert failed["exception"].endswith("ValueError: boom"), failed
    assert failed["request_id"] == access["request_id"]


def test_probe_answers_over_the_run_directory(tmp_path: Path) -> None:
    muructl = support.muructl_path()
    assert muructl is not None
    serve = start(tmp_path, "--admin-listen", "off")

    try:
        ready = subprocess.run(
            [str(muructl), "probe", "ready", "--run-dir", str(serve.run_dir)],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert ready.returncode == 0, ready.stdout
        assert ready.stdout.strip() == "ok"

        # A command inside the container finds the serve's admin socket
        # from the run directory.

        status = subprocess.run(
            [str(muructl), "status"],
            capture_output=True,
            text=True,
            timeout=30,
            env={**os.environ, "MURU_RUN_DIR": str(serve.run_dir)},
        )
        assert status.returncode == 0, status.stdout + status.stderr
        assert "plain-wsgi" in status.stdout, status.stdout
    finally:
        assert serve.stop() == 0

    gone = subprocess.run(
        [str(muructl), "probe", "live", "--run-dir", str(serve.run_dir)],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert gone.returncode == 1


def test_an_unwritable_run_directory_names_the_volume_it_needs(tmp_path: Path) -> None:
    if os.geteuid() == 0:
        pytest.skip("root can write anywhere")

    locked = tmp_path / "locked"
    locked.mkdir()
    locked.chmod(0o500)

    try:
        serve = start(tmp_path, "--run-dir", str(locked / "muru"), wait=False)
        assert serve.process.wait(timeout=30) != 0
        time.sleep(0.2)
        message = serve.output() + "".join(serve.stderr)
        assert "cannot be written" in message, message
        assert "emptyDir" in message, message
    finally:
        locked.chmod(0o700)


def test_a_missing_runtime_library_is_not_fetched(tmp_path: Path) -> None:
    base = getattr(sys, "_base_executable", sys.executable)

    if base == sys.executable:
        pytest.skip("the tests run outside a virtual environment")

    probe = subprocess.run([base, "-c", "import muru"], capture_output=True)

    if probe.returncode == 0:
        pytest.skip("the base interpreter has muru installed")

    serve = start(
        tmp_path,
        "--python",
        base,
        "--runtime-dir",
        str(tmp_path / "empty"),
        wait=False,
    )
    assert serve.process.wait(timeout=60) != 0
    time.sleep(0.2)
    message = serve.output() + "".join(serve.stderr)
    assert "no runtime library for" in message, message
    assert "muructl build --runtime-dir" in message, message
