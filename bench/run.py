"""Run the benchmarks: the same WSGI applications under several servers.

Each server is started in turn, warmed up, measured with oha, and stopped.
Results go to bench/results/<timestamp>.json and a summary table is printed
with each server's throughput relative to the best for that scenario.

    just bench                              # everything that is installed
    just bench --servers muru,gunicorn-gthread --scenarios hello
    just bench --duration 20s --connections 128 --workers 4 --threads 4

Muru is run three ways: `muru` with the access log off, as the other servers
are run; `muru-logged` with the access log written to the terminal, as
`muructl run` does by default; and `muru-baseline`, an earlier build given
with --baseline-muructl and --baseline-python, to see what has been added to
the cost of a request since then.

Servers whose command is not installed are skipped with a note. mod_wsgi-express
needs Apache httpd and the mod_wsgi package; see bench/README.md.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
APPS = ROOT / "bench" / "apps"
RESULTS = ROOT / "bench" / "results"

SCENARIOS = {
    "hello": ("hello", "/"),
    "large": ("large", "/"),
    "chunks": ("chunks", "/"),
}


def muructl_path() -> Path:
    return Path(os.environ.get("MURUCTL", ROOT / "target" / "release" / "muructl"))


# An earlier muructl, and an interpreter with that build's runtime library,
# for the muru-baseline server.

BASELINE: dict[str, Path] = {}


def run_flags(muructl: Path) -> str:
    """The help text of `muructl run`, to tell which flags a build takes."""

    completed = subprocess.run(
        [str(muructl), "run", "--help"], capture_output=True, text=True, check=False
    )
    return completed.stdout


def muru_command(
    muructl: Path,
    python: str,
    app: str,
    address: str,
    workers: int,
    threads: int,
    access_log: bool,
) -> list[str]:
    """The command line for one Muru build, with the flags that build takes:
    phase 0 named the processes --workers and had no access log."""

    flags = run_flags(muructl)
    command = [
        str(muructl),
        "run",
        app,
        "--bind",
        address,
        "--processes" if "--processes" in flags else "--workers",
        str(workers),
        "--threads",
        str(threads),
        "--python",
        python,
        "--chdir",
        str(APPS),
    ]

    if not access_log and "--access-log" in flags:
        command += ["--access-log", "off"]

    return command


@dataclass
class Server:
    """How to start one server for an application module."""

    name: str
    available: str  # an executable, or "python:module" for an importable module

    def command(
        self, module: str, port: int, workers: int, threads: int, scratch: Path
    ) -> list[str]:
        """Return the command line for this server."""

        app = f"{module}:application"
        address = f"127.0.0.1:{port}"
        python = sys.executable

        if self.name in ("muru", "muru-logged"):
            return muru_command(
                muructl_path(),
                python,
                app,
                address,
                workers,
                threads,
                access_log=self.name == "muru-logged",
            )

        if self.name == "muru-baseline":
            return muru_command(
                BASELINE["muructl"],
                str(BASELINE["python"]),
                app,
                address,
                workers,
                threads,
                access_log=False,
            )

        if self.name == "gunicorn-sync":
            return [
                python,
                "-m",
                "gunicorn",
                "--chdir",
                str(APPS),
                "-w",
                str(workers),
                "-b",
                address,
                "--log-level",
                "warning",
                app,
            ]

        if self.name == "gunicorn-gthread":
            return [
                python,
                "-m",
                "gunicorn",
                "--chdir",
                str(APPS),
                "-w",
                str(workers),
                "-k",
                "gthread",
                "--threads",
                str(threads),
                "-b",
                address,
                "--log-level",
                "warning",
                app,
            ]

        if self.name == "uvicorn":
            return [
                python,
                "-m",
                "uvicorn",
                "--interface",
                "wsgi",
                "--app-dir",
                str(APPS),
                "--workers",
                str(workers),
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--log-level",
                "warning",
                app,
            ]

        if self.name == "granian":
            return [
                python,
                "-m",
                "granian",
                "--interface",
                "wsgi",
                "--workers",
                str(workers),
                "--blocking-threads",
                str(threads),
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--log-level",
                "warning",
                app,
            ]

        if self.name.startswith("mod_wsgi"):
            command = [
                "mod_wsgi-express",
                "start-server",
                "--application-type",
                "module",
                "--entry-point",
                module,
                "--working-directory",
                str(APPS),
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--server-root",
                str(scratch / self.name),
                "--log-to-terminal",
                "--log-level",
                "warn",
                "--processes",
                str(workers),
                "--threads",
                str(threads),
            ]

            if self.name == "mod_wsgi-embedded":
                command += ["--embedded-mode"]
            elif self.name == "mod_wsgi-daemon-tuned":
                command += ["--disable-reloading", "--queue-timeout", "0"]

            return command

        raise ValueError(self.name)


SERVERS = [
    Server("muru", "muructl"),
    Server("muru-logged", "muructl"),
    Server("muru-baseline", "baseline"),
    Server("gunicorn-sync", "python:gunicorn"),
    Server("gunicorn-gthread", "python:gunicorn"),
    Server("uvicorn", "python:uvicorn"),
    Server("granian", "python:granian"),
    Server("mod_wsgi-embedded", "mod_wsgi-express"),
    Server("mod_wsgi-daemon", "mod_wsgi-express"),
    Server("mod_wsgi-daemon-tuned", "mod_wsgi-express"),
]


def is_available(server: Server) -> bool:
    if server.available == "muructl":
        return muructl_path().is_file()

    if server.available == "baseline":
        return bool(BASELINE) and all(path.is_file() for path in BASELINE.values())

    if server.available.startswith("python:"):
        module = server.available.split(":", 1)[1]
        probe = subprocess.run(
            [sys.executable, "-c", f"import {module}"], capture_output=True, check=False
        )
        return probe.returncode == 0

    return shutil.which(server.available) is not None


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def wait_for_port(port: int, process: subprocess.Popen[bytes], timeout: float = 60.0) -> None:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"server exited early with {process.returncode}")

        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)

    raise RuntimeError("server did not start listening in time")


def stop(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is None:
        process.send_signal(signal.SIGINT)

        try:
            process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def oha(url: str, duration: str, connections: int) -> dict[str, Any]:
    """Run oha against the URL and return its JSON report."""

    command = [
        "oha",
        "-z",
        duration,
        "-c",
        str(connections),
        "--no-tui",
        "--output-format",
        "json",
        url,
    ]
    completed = subprocess.run(command, capture_output=True, text=True, check=False)
    if completed.returncode != 0:
        raise RuntimeError(f"oha failed: {completed.stderr.strip()}")
    report: dict[str, Any] = json.loads(completed.stdout)
    return report


def tail(path: Path, lines: int = 20) -> str:
    """The last lines a server wrote, for when it failed."""

    try:
        return "\n".join(path.read_text(errors="replace").splitlines()[-lines:])
    except OSError:
        return ""


def summarise(report: dict[str, Any]) -> dict[str, Any]:
    summary = report.get("summary", {})
    percentiles = report.get("latencyPercentiles", {})
    return {
        "requests_per_second": summary.get("requestsPerSec"),
        "success_rate": summary.get("successRate"),
        "total": summary.get("total"),
        "p50_ms": (percentiles.get("p50") or 0) * 1000,
        "p90_ms": (percentiles.get("p90") or 0) * 1000,
        "p99_ms": (percentiles.get("p99") or 0) * 1000,
        "status_codes": report.get("statusCodeDistribution", {}),
        "errors": report.get("errorDistribution", {}),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--servers", default=",".join(server.name for server in SERVERS))
    parser.add_argument("--scenarios", default=",".join(SCENARIOS))
    parser.add_argument("--duration", default="10s")
    parser.add_argument("--connections", type=int, default=64)
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--warmup", default="2s")
    parser.add_argument(
        "--repeat",
        type=int,
        default=1,
        help="measure each server this many times and keep the run with the median throughput",
    )
    parser.add_argument(
        "--pause",
        type=float,
        default=0.0,
        help="seconds to wait between servers, for TIME_WAIT sockets to drain (macOS needs ~15)",
    )
    parser.add_argument(
        "--baseline-muructl", type=Path, help="an earlier muructl, run as muru-baseline"
    )
    parser.add_argument(
        "--baseline-python",
        type=Path,
        help="an interpreter with the earlier build's runtime library installed",
    )
    parser.add_argument("--list", action="store_true", help="show available servers and exit")
    args = parser.parse_args(argv)

    if args.baseline_muructl and args.baseline_python:
        BASELINE.update(muructl=args.baseline_muructl, python=args.baseline_python)

    if args.list:
        for server in SERVERS:
            state = "available" if is_available(server) else "missing"
            print(f"{server.name:24} {state}")

        return 0

    if shutil.which("oha") is None:
        print("oha is required: brew install oha, or cargo install oha", file=sys.stderr)
        return 2

    wanted = [server for server in SERVERS if server.name in args.servers.split(",")]
    results: dict[str, Any] = {
        "started": datetime.now(UTC).isoformat(),
        "platform": sys.platform,
        "python": sys.version.split()[0],
        "workers": args.workers,
        "threads": args.threads,
        "connections": args.connections,
        "duration": args.duration,
        "repeat": args.repeat,
        "baseline": {name: str(path) for name, path in BASELINE.items()},
        "runs": [],
    }
    scratch = Path(tempfile.mkdtemp(prefix="muru-bench-"))

    for scenario in args.scenarios.split(","):
        module, path = SCENARIOS[scenario]

        for server in wanted:
            if not is_available(server):
                print(f"skip {server.name}: not installed", file=sys.stderr)
                continue

            port = free_port()
            command = server.command(module, port, args.workers, args.threads, scratch)
            print(f"=== {scenario} on {server.name}", file=sys.stderr)
            output = scratch / f"{scenario}-{server.name}.log"

            with output.open("wb") as log:
                process = subprocess.Popen(command, cwd=APPS, stdout=log, stderr=log)

            try:
                wait_for_port(port, process)
                url = f"http://127.0.0.1:{port}{path}"
                oha(url, args.warmup, args.connections)
                reports = [oha(url, args.duration, args.connections) for _ in range(args.repeat)]
                reports.sort(key=lambda report: report["summary"].get("requestsPerSec") or 0)
                report = reports[len(reports) // 2]
            except Exception as error:
                print(f"failed {server.name}: {error}", file=sys.stderr)
                print(tail(output), file=sys.stderr)
                results["runs"].append(
                    {"scenario": scenario, "server": server.name, "error": str(error)}
                )
                continue
            finally:
                stop(process)
                time.sleep(args.pause)

            run = {"scenario": scenario, "server": server.name, **summarise(report)}
            results["runs"].append(run)
            print(
                f"{run['requests_per_second']:10.0f} req/s  p50 {run['p50_ms']:.2f} ms  "
                f"p99 {run['p99_ms']:.2f} ms  success {run['success_rate']:.3f}",
                file=sys.stderr,
            )

    shutil.rmtree(scratch, ignore_errors=True)

    RESULTS.mkdir(exist_ok=True)
    out = RESULTS / f"{datetime.now(UTC):%Y%m%dT%H%M%SZ}.json"
    out.write_text(json.dumps(results, indent=2))
    print(f"\nresults written to {out}\n")
    print_table(results)
    return 0


def print_table(results: dict[str, Any]) -> None:
    header = (
        f"{'scenario':8} {'server':24} {'req/s':>10} {'relative':>9} {'p50 ms':>8} {'p99 ms':>8}"
    )
    print(header)

    for scenario in SCENARIOS:
        runs = [
            run for run in results["runs"] if run["scenario"] == scenario and "error" not in run
        ]
        if not runs:
            continue
        best = max(run["requests_per_second"] for run in runs)

        for run in sorted(runs, key=lambda run: -run["requests_per_second"]):
            relative = run["requests_per_second"] / best if best else 0
            print(
                f"{scenario:8} {run['server']:24} {run['requests_per_second']:10.0f} "
                f"{relative:9.2f} {run['p50_ms']:8.2f} {run['p99_ms']:8.2f}"
            )


if __name__ == "__main__":
    sys.exit(main())
