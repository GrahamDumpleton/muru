"""Managed HTTP server releases (design specification section 9.9).

A release with kind = 'http' runs an ordinary HTTP server that the daemon
starts, probes, supervises and stops, and that the edge routes to like an
upstream, so releases, traffic policies, drains and rollbacks apply to it
as to native workers. The intended use is a cutover: the existing server
runs unchanged under Muru first, then traffic is canaried to native workers.
"""

from __future__ import annotations

import json
import os
import sys
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import SHOP, Daemon, start_daemon
from support.daemon import daemon as daemon_fixture
from support.load import Load

daemon = daemon_fixture

LEGACY = support.ROOT / "tests" / "fixtures" / "projects" / "legacy"
SERVER = [sys.executable, "server.py", "--bind", "{bind}"]


def wait_for(condition: Callable[[], bool], timeout: float = 15) -> None:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if condition():
            return

        time.sleep(0.1)

    assert condition()


def deploy_server(daemon: Daemon, app: str, *flags: str, command: list[str] | None = None) -> str:
    """Deploy the legacy server as a generic managed release and return the line printed."""

    arguments = ["deploy", app, str(LEGACY), "--server", "generic", "--stop-signal", "TERM"]
    completed = daemon.muructl(*arguments, *flags, "--", *(command or SERVER))
    return completed.stdout.strip()


def report(daemon: Daemon, path: str = "/", host: str | None = None, **headers: str) -> dict:
    response = daemon.request(path, host=host, headers=headers)
    assert response.status == 200, (response.status, response.body)
    return json.loads(response.body)


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False

    return True


def test_a_generic_server_is_started_probed_and_served(daemon: Daemon) -> None:
    line = deploy_server(daemon, "legacy", "--readiness", "/healthz")
    assert line == "muru: release legacy@r1 is ready and serving"

    # The request reaches the server re-serialised, with the forwarding
    # headers of section 4.13, in the directory and environment given.

    seen = report(daemon, "/catalogue?page=2")
    assert seen["server"] == "legacy"
    assert seen["path"] == "/catalogue?page=2"
    assert seen["headers"]["x-forwarded-for"] == "127.0.0.1"
    assert seen["headers"]["x-forwarded-proto"] == "http"
    assert "x-request-id" in seen["headers"]
    assert seen["cwd"] == str(LEGACY)
    assert seen["env"]["MURU_HTTP_BIND"].startswith("unix:")
    assert seen["env"]["MURU_INSTANCE"] == "0"
    assert seen["env"]["PORT"] is None

    # The release is listed by its server, the server's output is in the
    # release's log, and the health of its instance is a gauge.

    rows = daemon.muructl("release", "list").stdout.splitlines()
    assert any(row.split()[3:5] == ["1", "generic"] for row in rows[1:]), rows
    wait_for(lambda: "legacy listening on unix:" in daemon.muructl("logs", "legacy").stdout)
    metrics = daemon.muructl("metrics").stdout
    assert 'muru_upstream_health{upstream="legacy@r1",address="unix:' in metrics
    assert "} 1\n" in metrics.split('muru_upstream_health{upstream="legacy@r1"')[1]
    assert 'muru_workers_ready{app="legacy",release="r1",group="http"} 1' in metrics

    # The server's X-Muru-Redirect is honoured, as an application's is.

    assert report(daemon, "/redirect")["path"] == "/info"

    # The access log names the release and its instance.

    wait_for(lambda: "/catalogue" in daemon.muructl("logs", "--access").stdout)
    record = next(
        json.loads(line)
        for line in daemon.muructl("logs", "--access", "--json").stdout.splitlines()
        if '"/catalogue?page=2"' in line
    )
    assert (record["release"], record["group"]) == ("r1", "http")
    assert record["mount"].startswith("unix:")


def test_instances_bind_ports_and_their_count_is_fixed(daemon: Daemon) -> None:
    deploy_server(daemon, "legacy", "--bind", "port", "--instances", "2")
    reports = [report(daemon, f"/n/{n}") for n in range(12)]
    pids = {seen["pid"] for seen in reports}
    assert len(pids) == 2, reports
    first = reports[0]
    port = first["env"]["MURU_HTTP_BIND"].rsplit(":", 1)[1]
    assert first["env"]["MURU_HTTP_BIND"].startswith("127.0.0.1:")
    assert first["env"]["PORT"] == port
    assert {seen["env"]["MURU_INSTANCE"] for seen in reports} == {"0", "1"}

    refused = daemon.muructl("scale", "legacy@r1", "--processes", "3", check=False)
    assert refused.returncode != 0
    assert "instance count is fixed" in refused.stdout + refused.stderr


def test_readiness_waits_for_the_path_and_gives_up_in_time(daemon: Daemon, tmp_path: Path) -> None:
    flag = tmp_path / "ready"
    started = time.monotonic()
    failed = daemon.muructl(
        "deploy",
        "legacy",
        str(LEGACY),
        "--server",
        "generic",
        "--stop-signal",
        "TERM",
        "--readiness",
        "/healthz",
        "--readiness-timeout",
        "2s",
        "--env",
        f"LEGACY_READY={flag}",
        "--",
        *SERVER,
        check=False,
    )
    assert failed.returncode != 0
    assert "instance 0 was not ready within 2 s: /healthz did not answer 2xx or 3xx" in (
        failed.stdout + failed.stderr
    )
    assert time.monotonic() - started < 15
    assert daemon.releases("legacy") == [{"release": "legacy@r1", "state": "failed", "serving": ""}]

    # The next release becomes ready once the probe passes.

    recorded = daemon.muructl(
        "deploy",
        "legacy",
        str(LEGACY),
        "--server",
        "generic",
        "--stop-signal",
        "TERM",
        "--readiness",
        "/healthz",
        "--env",
        f"LEGACY_READY={flag}",
        "--no-wait",
        "--",
        *SERVER,
    )
    assert "legacy@r2 recorded" in recorded.stdout
    wait_for(lambda: daemon.releases("legacy")[0]["state"] == "warming")
    flag.write_text("go")
    daemon.muructl("release", "wait", "legacy@r2", "--timeout", "30")
    assert daemon.releases("legacy")[0] == {
        "release": "legacy@r2",
        "state": "ready",
        "serving": "yes",
    }
    assert report(daemon)["server"] == "legacy"


def test_a_crashed_instance_is_started_again(daemon: Daemon) -> None:
    deploy_server(daemon, "legacy")
    before = report(daemon)["pid"]
    assert report(daemon, "/crash")["pid"] == before
    wait_for(lambda: not alive(before))
    wait_for(
        lambda: any(
            "server.exited" in line for line in daemon.muructl("events").stdout.splitlines()
        )
    )
    wait_for(
        lambda: any(
            "server.started" in line for line in daemon.muructl("events").stdout.splitlines()
        ),
        20,
    )
    wait_for(lambda: daemon.request("/").status == 200, 20)
    after = report(daemon)["pid"]
    assert after != before
    events = daemon.muructl("events").stdout
    assert "instance 0 exited with exit status: 3; starting it again in 1s" in events
    assert f"instance 0 started again (pid {after})" in events
    assert daemon.releases("legacy") == [
        {"release": "legacy@r1", "state": "ready", "serving": "yes"}
    ]


def test_gunicorn_and_uvicorn_profiles_add_their_own_flags(daemon: Daemon, tmp_path: Path) -> None:
    for module in ("gunicorn", "uvicorn"):
        try:
            __import__(module)
        except ImportError:
            support.unavailable(f"{module} is not installed")

    line = daemon.muructl(
        "deploy",
        "gun",
        str(LEGACY),
        "--server",
        "gunicorn",
        "--",
        sys.executable,
        "-m",
        "gunicorn",
        "--workers",
        "1",
        "legacy:application",
    ).stdout.strip()
    assert line == "muru: release gun@r1 is ready and serving"
    line = daemon.muructl(
        "deploy",
        "uvi",
        str(LEGACY),
        "--server",
        "uvicorn",
        "--bind",
        "port",
        "--",
        sys.executable,
        "-m",
        "uvicorn",
        "asgi:app",
    ).stdout.strip()
    assert line == "muru: release uvi@r1 is ready and serving"

    config = tmp_path / "hosts.toml"
    config.write_text(
        """[host.gun]
names = ["gun.test"]

[host.uvi]
names = ["uvi.test"]

[[route]]
host = "gun"
app = "gun"

[[route]]
host = "uvi"
app = "uvi"
"""
    )
    daemon.muructl("apply", "-f", str(config))
    gun = report(daemon, "/shop", host="gun.test")
    assert (gun["server"], gun["path"], gun["forwarded_for"]) == ("gunicorn", "/shop", "127.0.0.1")
    uvi = report(daemon, "/api", host="uvi.test")
    assert (uvi["server"], uvi["path"]) == ("uvicorn", "/api")
    rows = {
        row.split()[0]: row.split()[3:5]
        for row in daemon.muructl("release", "list").stdout.splitlines()[1:]
    }
    assert rows == {"gun@r1": ["1", "gunicorn"], "uvi@r1": ["1", "uvicorn"]}


def test_a_cutover_canaries_from_the_server_to_native_workers(
    daemon: Daemon, tmp_path: Path
) -> None:
    deploy_server(daemon, "legacy")
    config = tmp_path / "hosts.toml"
    config.write_text(
        """[host.legacy]
names = ["legacy.test"]
default = true

[[route]]
host = "legacy"
app = "legacy"
"""
    )
    daemon.muructl("apply", "-f", str(config))

    # The native release of the same application is held, then canaried
    # against the server; both answer during the rollout.

    held = daemon.muructl(
        "deploy", "legacy", str(SHOP), "--python", sys.executable, "--hold"
    ).stdout.strip()
    assert held == "muru: release legacy@r2 is ready and held: not serving"
    daemon.muructl(
        "rollout", "canary", "legacy@r2", "--steps", "50", "--hold", "2s", "--min-requests", "20"
    )

    def served_by(cookie: str) -> str:
        response = daemon.request("/", host="legacy.test", headers={"Cookie": cookie})
        assert response.status == 200, response.body
        body = json.loads(response.body)
        return "legacy" if body.get("server") == "legacy" else "shop"

    seen = {served_by(f"muru_id=cutover-{n}") for n in range(40)}
    assert seen == {"legacy", "shop"}

    # The gates pass and the native release is promoted; the server is
    # drained and kept warm, and a rollback brings it straight back.

    def states() -> dict[str, tuple[str, str]]:
        return {row["release"]: (row["state"], row["serving"]) for row in daemon.releases("legacy")}

    wait_for(lambda: states().get("legacy@r2") == ("ready", "yes"), 60)
    wait_for(lambda: states().get("legacy@r1") == ("retained", ""), 30)
    assert json.loads(daemon.request("/", host="legacy.test").body).get("server") != "legacy"
    pid = None

    rolled = daemon.muructl("rollback", "legacy").stdout
    assert "legacy@r1" in rolled
    assert report(daemon, host="legacy.test")["server"] == "legacy"
    pid = report(daemon, host="legacy.test")["pid"]
    wait_for(lambda: states().get("legacy@r2") == ("retained", ""), 30)

    # Drained for good, the instance is stopped and its socket removed.

    daemon.muructl("drain", "legacy@r1")
    wait_for(lambda: states().get("legacy@r1") == ("stopped", ""), 30)
    wait_for(lambda: not alive(pid))
    assert not list((daemon.run_dir / "apps" / "legacy" / "r1").glob("**/server-*.sock"))


def test_a_gunicorn_deployment_cuts_over_to_native_workers_under_load(
    daemon: Daemon, tmp_path: Path
) -> None:
    """The exit criterion of phase 3 (section 14): an application served
    by gunicorn runs unchanged as a managed release, then is canaried to
    native workers, promoted, and its server drained, with no request
    failing throughout."""

    try:
        __import__("gunicorn")
    except ImportError:
        support.unavailable("gunicorn is not installed")

    line = daemon.muructl(
        "deploy",
        "legacy",
        str(LEGACY),
        "--server",
        "gunicorn",
        "--",
        sys.executable,
        "-m",
        "gunicorn",
        "--workers",
        "2",
        "legacy:application",
    ).stdout.strip()
    assert line == "muru: release legacy@r1 is ready and serving"
    assert not report(daemon)["native"]

    def states() -> dict[str, tuple[str, str]]:
        return {row["release"]: (row["state"], row["serving"]) for row in daemon.releases("legacy")}

    with Load(daemon.port, clients=4) as load:
        # The same application on native workers, held, then canaried
        # against gunicorn and promoted once the gates pass.

        held = daemon.muructl(
            "deploy",
            "legacy",
            str(LEGACY),
            "--entrypoint",
            "legacy:application",
            "--python",
            sys.executable,
            "--hold",
        ).stdout.strip()
        assert held == "muru: release legacy@r2 is ready and held: not serving"
        daemon.muructl(
            "rollout",
            "canary",
            "legacy@r2",
            "--steps",
            "50",
            "--hold",
            "2s",
            "--min-requests",
            "20",
        )
        wait_for(lambda: states().get("legacy@r2") == ("ready", "yes"), 60)
        wait_for(lambda: states().get("legacy@r1") == ("retained", ""), 30)

        # gunicorn is drained for good.

        daemon.muructl("drain", "legacy@r1")
        wait_for(lambda: states().get("legacy@r1") == ("stopped", ""), 30)
        time.sleep(0.5)

    load.check("the cutover from gunicorn to native workers", at_least=200)
    assert report(daemon)["native"]


def test_declared_servers_are_applied_exported_and_checked(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "legacy.toml"
    config.write_text(
        f"""[app.legacy.release.r1]
kind = 'http'
profile = 'generic'
command = {json.dumps(SERVER)}
cwd = '{LEGACY}'
instances = 1
readiness = {{ path = '/healthz', timeout = '20s' }}
stop_signal = 'TERM'
env = {{ LEGACY_SETTING = 'declared' }}

[host.legacy]
names = ['legacy.test']
default = true

[[route]]
host = 'legacy'
app = 'legacy'
"""
    )
    planned = daemon.muructl("apply", "-f", str(config), "--dry-run", check=False).stdout
    assert "generic, 1 instance in" in planned, planned
    daemon.muructl("apply", "-f", str(config))
    assert report(daemon, host="legacy.test")["env"]["LEGACY_SETTING"] == "declared"

    exported = daemon.muructl("config", "export").stdout
    assert "kind = 'http'" in exported.replace('"', "'")
    assert "profile = 'generic'" in exported.replace('"', "'")
    assert "stop_signal = 'TERM'" in exported.replace('"', "'")
    assert "entrypoint" not in exported
    again = tmp_path / "again.toml"
    again.write_text(exported)
    assert "no changes" in daemon.muructl("apply", "-f", str(again)).stdout

    def refused(table: str) -> str:
        bad = tmp_path / "bad.toml"
        bad.write_text(f"[app.bad.release.r1]\n{table}\n")
        completed = daemon.muructl("apply", "-f", str(bad), check=False)
        assert completed.returncode != 0
        return completed.stdout + completed.stderr

    base = f"kind = 'http'\ncommand = ['x']\ncwd = '{LEGACY}'\n"
    assert "profile: a managed HTTP release names its server" in refused(base)
    assert "stop_signal: the generic profile needs" in refused(base + "profile = 'generic'\n")
    assert "processes does not apply to a managed HTTP release" in refused(
        base + "profile = 'gunicorn'\nprocesses = 2\n"
    )
    assert "mod_wsgi-express binds a port only" in refused(base + "profile = 'mod_wsgi-express'\n")
    assert 'readiness.path "healthz" must start with /' in refused(
        base + "profile = 'gunicorn'\nreadiness = { path = 'healthz' }\n"
    )
    assert "instances must be at least 1" in refused(base + "profile = 'gunicorn'\ninstances = 0\n")
    assert "stop_signal must be one of" in refused(
        base + "profile = 'gunicorn'\nstop_signal = 'KILL'\n"
    )
    assert "profile applies to a managed HTTP release" in refused(
        f"source = '{SHOP}'\nprofile = 'gunicorn'\n"
    )
    assert "a managed HTTP release needs the command" in refused(
        f"kind = 'http'\nprofile = 'gunicorn'\ncwd = '{LEGACY}'\n"
    )


def test_a_daemon_restart_readopts_the_instances(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        deploy_server(first, "legacy", "--bind", "port")
        pid = report(first)["pid"]

        # The daemon detaches; the instance and the edge stay up.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        assert report(first)["pid"] == pid

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        assert report(second)["pid"] == pid
        assert second.releases("legacy") == [
            {"release": "legacy@r1", "state": "ready", "serving": "yes"}
        ]
        assert any(
            "re-adopted the server instances" in line
            for line in second.muructl("events").stdout.splitlines()
        )

        # An adopted instance that crashes is started again by the new
        # daemon, as its own child.

        assert report(second, "/crash")["pid"] == pid
        wait_for(lambda: not alive(pid))
        wait_for(lambda: second.request("/").status == 200 and report(second)["pid"] != pid, 20)
        replacement = report(second)["pid"]
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        second.stop(shutdown=True)

    wait_for(lambda: not alive(replacement))
