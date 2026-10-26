"""Routing on a client class (design specification section 4.14).

A route may be for some classes, a traffic policy may give a class a
release of its own, and a worker group may take a class's requests, so
crawlers can be kept off the capacity that serves people. The daemon's
file defines the classes; applied files may name only those.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

from support.daemon import Daemon, start_daemon

CLASSES = """
[[client_class]]
name = 'ai_crawler'
user_agent = ['GPTBot']

[[client_class]]
name = 'partner'
headers = { 'X-Partner' = '^yes$' }
"""

APP = """
import json

from muru import context


def application(environ, start_response):
    body = json.dumps(
        {
            "class": environ.get("muru.client_class"),
            "release": context().release,
            "group": context().group,
        }
    ).encode()
    start_response("200 OK", [("Content-Type", "application/json")])
    return [body]
"""


def write_site(base: Path, port: int, route_extra: str = "") -> Path:
    source = base / "source"
    source.mkdir(exist_ok=True)
    (source / "classapp.py").write_text(APP)
    archive = base / "archive"
    archive.mkdir(exist_ok=True)
    (archive / "index.html").write_text("archived\n")
    release = f"""source = '{source}'
entrypoint = 'classapp:application'
python = '{sys.executable}'
processes = 1
threads = 2
"""
    config = base / "site.toml"
    config.write_text(
        f"""[app.site.release.r1]
{release}
[app.site.release.r1.groups.crawlers]
match = {{ client_class = ['ai_crawler'] }}
processes = 1

[app.site.release.r1.groups.hot]
paths = ['/status']
processes = 1

[app.site.release.r2]
{release}
[host.site]
names = ['site.test']
listeners = ['127.0.0.1:{port}']
default = true

[[route]]
host = 'site'
app = 'site'
traffic = {{ classes = {{ ai_crawler = 'r1' }} }}

[[route]]
host = 'site'
static = '{archive}'
match = {{ client_class = ['partner'] }}
{route_extra}"""
    )
    return config


def seen(daemon: Daemon, path: str, headers: dict[str, str] | None = None) -> dict[str, str]:
    response = daemon.request(path, host="site.test", headers=headers)
    assert response.status == 200, response.body
    report: dict[str, str] = json.loads(response.body)
    return report


def test_routes_policies_and_groups_take_a_class(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra=CLASSES)

    try:
        applied = daemon.muructl("apply", "-f", str(write_site(tmp_path, daemon.port)))
        assert applied.returncode == 0, applied.stdout + applied.stderr
        daemon.muructl("release", "wait", "site@r2", "--timeout", "60")
        daemon.muructl("release", "wait", "site@r1", "--timeout", "60")

        # People reach the serving release; the policy gives crawlers
        # r1, kept running for them, where their group takes every path.

        assert seen(daemon, "/") == {"class": "default", "release": "r2", "group": "default"}
        crawler = {"User-Agent": "Mozilla/5.0 GPTBot/1.2"}
        assert seen(daemon, "/", crawler) == {
            "class": "ai_crawler",
            "release": "r1",
            "group": "crawlers",
        }
        assert seen(daemon, "/status", crawler)["group"] == "crawlers"

        # A partner's requests take the route for its class.

        partner = daemon.request("/", host="site.test", headers={"X-Partner": "yes"})
        assert partner.status == 200 and partner.body == b"archived\n"

        listed = daemon.muructl("route", "list").stdout
        assert "site (ai_crawler to r1)" in listed, listed
        assert "[for partner]" in listed, listed

        # A class the daemon's file does not define is refused, as is a
        # second route for a class at the same path.

        refused = daemon.muructl(
            "apply",
            "-f",
            str(
                write_site(
                    tmp_path,
                    daemon.port,
                    "\n[[route]]\nhost = 'site'\napp = 'site'\n"
                    "match = { client_class = ['partner', 'robots'] }\n",
                )
            ),
            check=False,
        )
        assert refused.returncode != 0
        assert "no client class named robots" in refused.stdout + refused.stderr
        assert "another route with the path is for client class partner" in (
            refused.stdout + refused.stderr
        )
    finally:
        daemon.stop()
