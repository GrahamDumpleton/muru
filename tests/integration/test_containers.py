"""Muru as a container's main process (design specification sections 6.10
and 13.3), in a container built from the working tree. PID 1 is muructl's
built-in init, so what a killed worker leaves behind is collected and no
zombie remains, and `docker stop` means shutdown: a request in flight
completes, the container exits 0 inside the budget. Both hold for `muructl
run --allow-public` and for the daemon.

The image takes minutes to build the first time and is cached after; the
suite carries the requires_docker marker and runs with `just test-docker`."""

from __future__ import annotations

import http.client
import shutil
import subprocess
import threading
import time
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest

pytestmark = pytest.mark.requires_docker

ROOT = Path(__file__).resolve().parents[2]
IMAGE = "muru-test:working-tree"

# A short worker stop budget, so the scenarios run in seconds.

TIMEOUTS = ["--timeout", "hard=1s", "--timeout", "hard_drain_grace=1s", "--timeout", "shutdown=1s"]


def docker_available() -> bool:
    binary = shutil.which("docker")
    if binary is None:
        return False
    return subprocess.run([binary, "info"], capture_output=True, check=False).returncode == 0


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


@pytest.fixture(scope="module")
def image() -> str:
    """The test image, built from the working tree."""

    if not docker_available():
        pytest.fail("the container scenarios need a running container runtime")

    completed = subprocess.run(
        ["docker", "build", "-q", "-t", IMAGE, "-f", "images/test/Dockerfile", "."],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
        timeout=3600,
    )

    if completed.returncode != 0:
        pytest.fail(f"building {IMAGE} failed:\n{completed.stdout}{completed.stderr[-4000:]}")

    return IMAGE


class Container:
    """A container running muructl as its main process, reachable on a host port."""

    def __init__(self, name: str, port: int) -> None:
        self.name = name
        self.port = port

    def exec(self, *command: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        """Run a command in the container."""

        return subprocess.run(
            ["docker", "exec", self.name, *command],
            capture_output=True,
            text=True,
            check=check,
            timeout=120,
        )

    def processes(self) -> list[tuple[int, int, str, str]]:
        """Every process in the container: pid, parent pid, state and command."""

        listing = self.exec(
            "python3",
            "-c",
            "import os\n"
            "for entry in sorted(int(e) for e in os.listdir('/proc') if e.isdigit()):\n"
            "    try:\n"
            "        stat = open(f'/proc/{entry}/stat', 'rb').read()\n"
            "    except OSError:\n"
            "        continue\n"
            "    name = stat[stat.find(b'(') + 1 : stat.rfind(b')')].decode()\n"
            "    fields = stat[stat.rfind(b')') + 1 :].split()\n"
            "    print(entry, fields[1].decode(), fields[0].decode(), name)\n",
        ).stdout
        processes = []

        for line in listing.splitlines():
            pid, parent, state, name = line.split(" ", 3)
            processes.append((int(pid), int(parent), state, name))

        return processes

    def logs(self) -> str:
        completed = subprocess.run(
            ["docker", "logs", self.name], capture_output=True, text=True, check=False
        )
        return completed.stdout + completed.stderr

    def get(self, path: str) -> http.client.HTTPResponse:
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=60)
        connection.request("GET", path)
        response = connection.getresponse()
        response.read()
        connection.close()
        return response

    def request_in_the_background(self, path: str) -> tuple[threading.Thread, list[str]]:
        """Request a path on a thread of its own, recording the status or the error."""

        outcome: list[str] = []

        def request() -> None:
            try:
                outcome.append(f"status {self.get(path).status}")
            except (OSError, http.client.HTTPException) as error:
                outcome.append(type(error).__name__)

        thread = threading.Thread(target=request)
        thread.start()
        return thread, outcome

    def stop(self, grace: int = 10) -> tuple[float, int]:
        """docker stop with the grace, returning how long it took and the exit code."""

        started = time.monotonic()
        subprocess.run(
            ["docker", "stop", "-t", str(grace), self.name], check=True, capture_output=True
        )
        elapsed = time.monotonic() - started
        code = subprocess.run(
            ["docker", "inspect", "-f", "{{.State.ExitCode}}", self.name],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        return elapsed, int(code)

    def remove(self) -> None:
        subprocess.run(["docker", "rm", "-f", self.name], check=False, capture_output=True)


@pytest.fixture
def container(image: str, request: pytest.FixtureRequest) -> Iterator[Callable[..., Container]]:
    """Start containers from the image, removed when the test ends."""

    started: list[Container] = []

    def start(*command: str, env: dict[str, str] | None = None) -> Container:
        name = f"muru-test-{request.node.name[:40]}-{len(started)}".replace("[", "-").replace(
            "]", ""
        )
        subprocess.run(["docker", "rm", "-f", name], check=False, capture_output=True)
        environment = [f"--env={key}={value}" for key, value in (env or {}).items()]
        subprocess.run(
            [
                "docker",
                "run",
                "-d",
                "--name",
                name,
                "-p",
                "127.0.0.1::8080",
                *environment,
                image,
                *command,
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        mapping = subprocess.run(
            ["docker", "port", name, "8080"], capture_output=True, text=True, check=True
        ).stdout
        port = int(mapping.split("\n")[0].rsplit(":", 1)[1])
        container = Container(name, port)
        started.append(container)
        return container

    yield start

    for container in started:
        container.remove()


def wait_until_serving(container: Container) -> None:
    def answers() -> bool:
        try:
            return container.get("/").status == 200
        except OSError:
            return False

    deadline = time.monotonic() + 60

    while not answers():
        assert time.monotonic() < deadline, f"not serving:\n{container.logs()}"
        time.sleep(0.2)


def test_muructl_run_is_pid_1_with_the_init_and_leaves_no_zombie(
    container: Callable[..., Container],
) -> None:
    instance = container(
        "muructl",
        "run",
        "plain_wsgi:application",
        "--allow-public",
        "--bind",
        "0.0.0.0:8080",
        "--processes",
        "1",
        "--threads",
        "2",
        *TIMEOUTS,
    )
    wait_until_serving(instance)

    # PID 1 is the init, and the command runs as its child.

    processes = instance.processes()
    assert processes[0][:2] == (1, 0) and processes[0][3] == "muructl", processes
    assert any(pid != 1 and parent == 1 and name == "muructl" for pid, parent, _, name in processes)

    # A request starts a child and runs past the hard timeout, which kills
    # the worker; the host, a subreaper, stops the child and collects it,
    # and whatever else reparents to PID 1 is collected there.

    thread, _ = instance.request_in_the_background("/spawn?seconds=30&pidfile=/tmp/child.pid")
    wait_for(lambda: instance.exec("cat", "/tmp/child.pid", check=False).returncode == 0)
    child = int(instance.exec("cat", "/tmp/child.pid").stdout)
    wait_for(lambda: "worker.timeout_killed" in instance.logs())
    wait_for(lambda: child not in {pid for pid, _, _, _ in instance.processes()})
    thread.join(timeout=10)
    assert not [p for p in instance.processes() if p[2] == "Z"], instance.processes()
    assert instance.get("/").status == 200


def test_docker_stop_lets_a_request_finish_under_run(container: Callable[..., Container]) -> None:
    instance = container(
        "muructl",
        "run",
        "plain_wsgi:application",
        "--allow-public",
        "--bind",
        "0.0.0.0:8080",
        "--processes",
        "1",
        "--threads",
        "2",
    )
    wait_until_serving(instance)
    thread, outcome = instance.request_in_the_background("/sleep?seconds=3")
    time.sleep(0.5)

    # SIGTERM through the init: the request in flight completes and the
    # container exits 0 inside the budget, under docker stop's grace.

    elapsed, code = instance.stop()
    thread.join(timeout=10)
    assert outcome == ["status 200"]
    assert code == 0, instance.logs()
    assert elapsed < 9, elapsed


def test_docker_stop_drains_the_daemon(container: Callable[..., Container]) -> None:
    instance = container(
        "muructl",
        "daemon",
        "run",
        "--listen",
        "0.0.0.0:8080",
        "--state-dir",
        "/var/lib/muru",
        "--run-dir",
        "/run/muru",
        "--log-dir",
        "/var/log/muru",
    )
    wait_for(
        lambda: instance.exec("test", "-S", "/run/muru/admin.sock", check=False).returncode == 0
    )
    deployed = instance.exec(
        "muructl",
        "deploy",
        "shop",
        "/projects/shop",
        "--python",
        "/usr/local/bin/python3",
        "--processes",
        "1",
        "--socket",
        "/run/muru/admin.sock",
    ).stdout
    assert "ready and serving" in deployed, deployed
    wait_until_serving(instance)
    thread, outcome = instance.request_in_the_background("/slow?ms=3000")
    time.sleep(0.5)

    # As the container's main process the daemon shuts down on SIGTERM
    # rather than detaching: the request finishes, the workers and the
    # edge stop, and the container exits 0 inside the budget.

    elapsed, code = instance.stop()
    thread.join(timeout=10)
    assert outcome == ["status 200"]
    assert code == 0, instance.logs()
    assert elapsed < 9, elapsed
    assert "SIGTERM: shutting down" in instance.logs(), instance.logs()


def test_the_init_can_be_turned_off(container: Callable[..., Container]) -> None:
    instance = container(
        "muructl",
        "run",
        "plain_wsgi:application",
        "--allow-public",
        "--bind",
        "0.0.0.0:8080",
        "--processes",
        "1",
        env={"MURU_NO_INIT": "1"},
    )
    wait_until_serving(instance)

    # PID 1 runs the command itself, and a stop still drains.

    processes = instance.processes()
    assert processes[0][3] == "muructl"
    assert not any(pid != 1 and name == "muructl" for pid, _, _, name in processes), processes
    elapsed, code = instance.stop()
    assert code == 0, instance.logs()
    assert elapsed < 9, elapsed
