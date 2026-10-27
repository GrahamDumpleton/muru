"""The overlays' external trigger (design specification section 9.10): a
signed webhook on the address of the daemon's `[overlay_webhook]` table
activates and ends the overlays declared with `trigger.external = true`.
"""

from __future__ import annotations

import hashlib
import hmac
import http.client
import json
import shutil
import sys
import time
from collections.abc import Iterator
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon

FIXTURE = support.ROOT / "tests" / "fixtures" / "projects" / "clients"

SECRET = b"s3cret"


@dataclass
class Site:
    daemon: Daemon
    webhook_port: int

    def mode(self) -> str | None:
        connection = http.client.HTTPConnection("127.0.0.1", self.daemon.port, timeout=30)

        try:
            connection.request("GET", "/")
            return json.loads(connection.getresponse().read())["mode"]
        finally:
            connection.close()

    def trigger(
        self,
        overlay: str,
        body: dict[str, str],
        secret: bytes = SECRET,
        method: str = "POST",
    ) -> tuple[int, dict[str, str]]:
        data = json.dumps(body).encode()
        signature = "sha256=" + hmac.new(secret, data, hashlib.sha256).hexdigest()
        connection = http.client.HTTPConnection("127.0.0.1", self.webhook_port, timeout=30)

        try:
            connection.request(
                method,
                f"/v1/overlays/{overlay}:trigger",
                body=data,
                headers={"Content-Type": "application/json", "X-Muru-Signature": signature},
            )
            response = connection.getresponse()
            return response.status, json.loads(response.read())
        finally:
            connection.close()


def now(offset: float = 0) -> str:
    return (datetime.now(UTC) + timedelta(seconds=offset)).strftime("%Y-%m-%dT%H:%M:%SZ")


@pytest.fixture(scope="module")
def site(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Site]:
    base = tmp_path_factory.mktemp("webhook")
    project = base / "project"
    shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
    (project / "muru.toml").write_text('entrypoint = "clients:wsgi_app"\n')
    webhook_port = support.free_port()
    daemon = start_daemon(
        base,
        config_extra=f"""
[overlay_webhook]
listen = '127.0.0.1:{webhook_port}'
secret = '{SECRET.decode()}'
""",
    )

    try:
        config = base / "site.toml"
        config.write_text(
            f"""[app.clients.release.r1]
source = '{project}'
python = '{sys.executable}'
processes = 1

[host.site]
names = ['site.test']
listeners = ['127.0.0.1:{daemon.port}']
default = true

[[route]]
name = 'root'
host = 'site'
path = '/'
app = 'clients'

[overlay.outage]
routes.root = {{ mode = 'degraded' }}
trigger = {{ external = true }}
priority = 2

[overlay.manual]
routes.root = {{ mode = 'read-only' }}
priority = 1
"""
        )
        daemon.muructl("apply", "-f", str(config))
        daemon.muructl("release", "wait", "clients@r1", "--timeout", "60")
        yield Site(daemon, webhook_port)
    finally:
        daemon.stop()


def test_a_signed_request_activates_and_ends_an_overlay(site: Site) -> None:
    assert "by hand or webhook" in site.daemon.muructl("overlay", "list").stdout
    assert site.mode() is None

    status, answer = site.trigger("outage", {"action": "activate", "time": now(), "for": "1h"})
    assert status == 200, answer
    assert answer == {"overlay": "outage", "message": "activated by the webhook"}
    assert site.mode() == "degraded"
    assert "active (external)" in site.daemon.muructl("overlay", "list").stdout

    status, answer = site.trigger("outage", {"action": "activate", "time": now()})
    assert status == 409 and "already active" in answer["message"]

    status, answer = site.trigger("outage", {"action": "end", "time": now()})
    assert status == 200 and answer["message"] == "ended by the webhook"
    assert site.mode() is None


def test_an_activation_for_a_time_ends_on_its_own(site: Site) -> None:
    status, _ = site.trigger("outage", {"action": "activate", "time": now(), "for": "2s"})
    assert status == 200
    assert site.mode() == "degraded"
    time.sleep(3.5)
    assert site.mode() is None


def test_requests_that_are_not_signed_current_or_allowed_are_refused(site: Site) -> None:
    body = {"action": "activate", "time": now()}
    status, answer = site.trigger("outage", body, secret=b"wrong")
    assert (status, answer["error"]) == (401, "bad_signature")
    assert "the body's HMAC with the webhook's secret" in answer["message"]

    status, answer = site.trigger("outage", {"action": "activate", "time": now(-600)})
    assert (status, answer["error"]) == (401, "stale")

    # Only overlays that declare the external trigger take it.

    status, answer = site.trigger("manual", body)
    assert (status, answer["error"]) == (404, "not_found")
    assert "no overlay named manual takes the external trigger" in answer["message"]

    status, answer = site.trigger("outage", {"action": "promote", "time": now()})
    assert (status, answer["error"]) == (400, "bad_request")

    status, answer = site.trigger("outage", body, method="GET")
    assert status == 405
    assert site.mode() is None
