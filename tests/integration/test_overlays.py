"""Overlays (design specification section 9.10): named partial
configuration laid over routes and applications while active, by hand or
by a schedule's window, with preconditions, previews and promotion. The
base configuration is never changed until an overlay is promoted.

One daemon serves every test here, with two releases of an application
that reports the release and mode it served with, behind a named route.
"""

from __future__ import annotations

import http.client
import json
import shutil
import sys
import time
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon

FIXTURE = support.ROOT / "tests" / "fixtures" / "projects" / "clients"


@dataclass
class Site:
    daemon: Daemon
    base: Path
    project: Path

    def request(self, method: str = "GET") -> support.Response:
        connection = http.client.HTTPConnection("127.0.0.1", self.daemon.port, timeout=30)

        try:
            connection.request(method, "/")
            raw = connection.getresponse()
            return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
        finally:
            connection.close()

    def seen(self) -> dict[str, str | None]:
        response = self.request()
        assert response.status == 200, response.body
        return json.loads(response.body)

    def overlay(self, *arguments: str, check: bool = True) -> str:
        completed = self.daemon.muructl("overlay", *arguments, check=check)
        return completed.stdout + completed.stderr

    def apply(self, overlays: str, check: bool = True) -> str:
        config = self.base / "site.toml"
        config.write_text(
            f"""[app.clients.release.r1]
source = '{self.project}'
python = '{sys.executable}'
processes = 1

[app.clients.release.r2]
source = '{self.project}'
python = '{sys.executable}'
processes = 1

[host.site]
names = ['site.test']
listeners = ['127.0.0.1:{self.daemon.port}']
default = true

[[route]]
name = 'root'
host = 'site'
path = '/'
app = 'clients'
"""
            + overlays
        )
        completed = self.daemon.muructl("apply", "-f", str(config), check=check)
        return completed.stdout + completed.stderr


def eventually(check: Callable[[], bool], timeout: float = 15.0) -> bool:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if check():
            return True

        time.sleep(0.2)

    return False


def at(seconds: float) -> str:
    """A moment `seconds` from now, as an RFC 3339 time."""

    return (datetime.now(UTC) + timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%SZ")


OVERLAYS = """
[overlay.read-only]
routes.root = { mode = 'read-only', deny = ['POST', 'PUT'] }

[overlay.pinned]
routes.root.traffic = { pin = 'r1' }
preconditions = { release_ready = 'r1' }
priority = 2

[overlay.broken]
routes.root.traffic = { split = { r9 = 50 } }
preconditions = { release_ready = 'r9' }
priority = 1
"""


@pytest.fixture(scope="module")
def site(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Site]:
    base = tmp_path_factory.mktemp("overlays")
    project = base / "project"
    shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
    (project / "muru.toml").write_text('entrypoint = "clients:wsgi_app"\n')
    daemon = start_daemon(base)

    try:
        site = Site(daemon, base, project)
        site.apply(OVERLAYS)
        daemon.muructl("release", "wait", "clients@r2", "--timeout", "60")
        assert eventually(lambda: site.seen()["release"] == "r2")
        yield site
    finally:
        daemon.stop()


def test_overlays_are_listed_idle_until_activated(site: Site) -> None:
    listed = site.overlay("list")
    assert "read-only" in listed and "idle" in listed
    assert "route root" in listed
    assert "by hand" in listed


def test_an_overlay_applies_while_active_and_the_base_shows_through_after(site: Site) -> None:
    assert site.seen()["mode"] is None
    preview = site.overlay("preview", "read-only")
    assert "route root: mode (none) -> read-only" in preview
    assert "route root: methods.deny (none) -> POST, PUT" in preview

    assert "activated by hand" in site.overlay("activate", "read-only")
    assert site.seen()["mode"] == "read-only"
    refused = site.request("POST")
    assert refused.status == 503
    assert b"read_only" in refused.body
    assert "active (manual)" in site.overlay("list")
    assert "already active" in site.overlay("activate", "read-only", check=False)

    assert "ended by hand" in site.overlay("end", "read-only")
    assert site.seen()["mode"] is None
    assert site.request("POST").status == 200
    assert "overlay.activated" in site.daemon.events()
    assert "overlay.ended" in site.daemon.events()


def test_an_activation_for_a_time_ends_on_its_own(site: Site) -> None:
    site.overlay("activate", "read-only", "--for", "2s")
    assert site.seen()["mode"] == "read-only"
    assert eventually(lambda: site.seen()["mode"] is None)


def test_a_pin_sends_every_request_to_one_release(site: Site) -> None:
    preview = site.overlay("preview", "pinned")
    assert "route root: traffic.split (none) -> r1 100%" in preview

    site.overlay("activate", "pinned")
    assert eventually(lambda: site.seen()["release"] == "r1")
    assert all(site.seen()["release"] == "r1" for _ in range(10))

    site.overlay("end", "pinned")
    assert eventually(lambda: site.seen()["release"] == "r2")


def test_failing_preconditions_keep_an_overlay_from_activating(site: Site) -> None:
    refused = site.overlay("activate", "broken", check=False)
    assert "was not activated: clients@r9 does not exist" in refused
    assert site.seen()["release"] == "r2"


def test_a_schedule_activates_and_ends_an_overlay_in_its_window(site: Site) -> None:
    window = f"""
[overlay.window]
apps.clients = {{ maintenance = true }}
trigger = {{ schedule = {{ start = '{at(3)}', end = '{at(7)}' }} }}
"""
    site.apply(OVERLAYS + window)
    assert "from " in site.overlay("list")
    assert site.request().status == 200
    assert eventually(lambda: site.request().status == 503)
    assert b"maintenance" in site.request().body
    assert eventually(lambda: site.request().status == 200)

    events = site.daemon.muructl("events").stdout
    assert "its schedule's window began" in events
    assert "ended: its schedule" in events


def test_promotion_makes_an_overlay_the_base(site: Site) -> None:
    site.apply(OVERLAYS)
    site.overlay("activate", "read-only")
    assert "promoted" in site.overlay("promote", "read-only")
    assert site.seen()["mode"] == "read-only"
    assert "promoted" in site.overlay("list")

    exported = site.daemon.muructl("config", "export").stdout
    assert 'mode = "read-only"' in exported
    assert "overlay.promoted" in site.daemon.events()


def test_apply_refuses_overlays_that_do_not_fit(site: Site) -> None:
    refused = site.apply(
        OVERLAYS
        + """
[overlay.clash]
routes.root = { mode = 'limited' }

[overlay.nowhere]
routes.missing = { mode = 'x' }
apps.elsewhere = { maintenance = true }
""",
        check=False,
    )
    assert "overlays clash, read-only all set routes.root.mode" in refused
    assert "overlay nowhere: no route is named missing" in refused
    assert "overlay nowhere: no application named elsewhere" in refused
