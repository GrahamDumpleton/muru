"""Client classes, rate limits, method restrictions and site modes
(design specification sections 4.14 and 4.15).

One daemon serves every test here. Its own file declares the classes,
one of them from a list file and one confirmed by DNS, and the rate
limits; the applied routes name the limits, restrict methods and carry a
mode. Every request comes from 127.0.0.1, whose name is localhost.
"""

from __future__ import annotations

import http.client
import json
import shutil
import sys
import time
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon

FIXTURE = support.ROOT / "tests" / "fixtures" / "projects" / "clients"

CLASSES = """
[[client_class]]
name = 'verified'
user_agent = ['Testbot']
verify = 'dns'
verify_domains = ['localhost']

[[client_class]]
name = 'ai_crawler'
user_agent_list = 'lists/ai.txt'

[[client_class]]
name = 'claimed'
user_agent = ['Testbot']

[[client_class]]
name = 'scored'
headers = { 'X-Bot-Score' = '^[0-9]$' }

[[client_class]]
name = 'partner'
addresses = ['127.0.0.1']
cookies = { tier = '^gold$' }

[rate_limit.crawlers]
match = { client_class = ['ai_crawler'] }
rate = '1/m'
burst = 2
headers = true

[rate_limit.slow]
rate = '4/s'
burst = 1
action = 'delay'
max_delay = '2s'

[rate_limit.watch]
match = { client_class = ['scored'] }
rate = '1/m'
burst = 1
action = 'log'
"""


@dataclass
class Site:
    daemon: Daemon
    base: Path
    metrics_port: int

    def request(
        self, path: str, headers: dict[str, str] | None = None, method: str = "GET"
    ) -> support.Response:
        connection = http.client.HTTPConnection("127.0.0.1", self.daemon.port, timeout=30)

        try:
            connection.request(method, path, headers=headers or {})
            raw = connection.getresponse()
            return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
        finally:
            connection.close()

    def seen(self, path: str, headers: dict[str, str] | None = None) -> dict[str, str | None]:
        response = self.request(path, headers)
        assert response.status == 200, response.body
        return json.loads(response.body)

    def metrics(self) -> str:
        connection = http.client.HTTPConnection("127.0.0.1", self.metrics_port, timeout=30)

        try:
            connection.request("GET", "/metrics")
            return connection.getresponse().read().decode()
        finally:
            connection.close()


def eventually(check: Callable[[], bool], timeout: float = 10.0) -> bool:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if check():
            return True

        time.sleep(0.1)

    return False


@pytest.fixture(scope="module")
def site(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Site]:
    base = tmp_path_factory.mktemp("clients")
    project = base / "project"
    shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
    (project / "muru.toml").write_text(
        'entrypoint = "clients:wsgi_app"\n\n'
        '[mounts.asgi]\npath = "/asgi"\nentrypoint = "clients:asgi_app"\ninterface = "asgi"\n'
    )
    (base / "lists").mkdir()
    (base / "lists" / "ai.txt").write_text("# AI crawlers\nGPTBot\n")
    metrics_port = support.free_port()
    daemon = start_daemon(
        base, config_extra=f"[metrics]\nlisten = '127.0.0.1:{metrics_port}'\n" + CLASSES
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
rate_limits = ['watch']

[[route]]
host = 'site'
path = '/'
app = 'clients'

[[route]]
host = 'site'
path = '/limited/'
app = 'clients'
rate_limits = ['crawlers']

[[route]]
host = 'site'
path = '/delayed/'
app = 'clients'
rate_limits = ['slow']

[[route]]
host = 'site'
path = '/readonly/'
app = 'clients'
methods = {{ deny = ['POST', 'PUT'] }}
mode = 'read-only'

[[route]]
host = 'site'
path = '/strict/'
app = 'clients'
methods = {{ deny = ['DELETE'], answer = 'method_not_allowed' }}

[[route]]
host = 'site'
path = '/asgi/'
app = 'clients'
mode = 'limited'
"""
        )
        daemon.muructl("apply", "-f", str(config))
        daemon.muructl("release", "wait", "clients@r1", "--timeout", "60")
        yield Site(daemon, base, metrics_port)
    finally:
        daemon.stop()


def test_the_first_rule_that_matches_names_the_class(site: Site) -> None:
    assert site.seen("/", {"User-Agent": "Mozilla/5.0 GPTBot/1.1"})["class"] == "ai_crawler"
    assert site.seen("/", {"X-Bot-Score": "5"})["class"] == "scored"
    assert site.seen("/", {"X-Bot-Score": "57"})["class"] == "default"
    assert site.seen("/", {"Cookie": "a=1; tier=gold"})["class"] == "partner"
    assert site.seen("/", {"User-Agent": "Mozilla/5.0"})["class"] == "default"

    # An ASGI application finds it in the scope, with the route's mode.

    assert site.seen("/asgi/", {"User-Agent": "GPTBot"}) == {
        "class": "ai_crawler",
        "mode": "limited",
        "method": "GET",
    }


def test_a_claimed_crawler_falls_through_until_dns_confirms_it(site: Site) -> None:
    headers = {"User-Agent": "Testbot/2.0"}
    assert site.seen("/", headers)["class"] in ("claimed", "verified")
    assert eventually(lambda: site.seen("/", headers)["class"] == "verified")


def test_a_rate_limit_refuses_with_retry_after_and_its_fields(site: Site) -> None:
    crawler = {"User-Agent": "GPTBot"}
    assert site.request("/limited/", crawler).status == 200
    assert site.request("/limited/", crawler).status == 200
    refused = site.request("/limited/", crawler)
    assert refused.status == 429
    assert b"rate_limited" in refused.body
    assert int(refused.header("Retry-After") or "0") >= 1
    assert refused.header("RateLimit-Policy") == '"crawlers";q=2;w=120'
    assert (refused.header("RateLimit") or "").startswith('"crawlers";r=0;t=')

    # Other classes do not draw from it.

    for _ in range(5):
        assert site.request("/limited/", {"User-Agent": "Mozilla/5.0"}).status == 200

    assert eventually(lambda: "ratelimit.enforcing" in site.daemon.events())
    assert (
        'muru_rate_limited_total{limit="crawlers",client_class="ai_crawler",action="rejected"} 1'
        in site.metrics()
    )


def test_a_delaying_limit_holds_the_request(site: Site) -> None:
    assert site.request("/delayed/").status == 200
    started = time.monotonic()
    assert site.request("/delayed/").status == 200
    assert time.monotonic() - started >= 0.15


def test_a_logging_limit_only_counts(site: Site) -> None:
    for _ in range(3):
        assert site.request("/", {"X-Bot-Score": "7"}).status == 200

    assert 'limit="watch",client_class="scored",action="logged"' in site.metrics()


def test_methods_are_refused_and_the_mode_reaches_the_application(site: Site) -> None:
    refused = site.request("/readonly/", method="POST")
    assert refused.status == 503
    assert b"read_only" in refused.body
    assert site.seen("/readonly/")["mode"] == "read-only"

    not_allowed = site.request("/strict/thing", method="DELETE")
    assert not_allowed.status == 405
    assert b"method_not_allowed" in not_allowed.body
    allow = not_allowed.header("Allow") or ""
    assert "GET" in allow
    assert "DELETE" not in allow

    seen = site.seen("/strict/thing")
    assert seen["mode"] is None
    assert seen["method"] == "GET"


def test_requests_are_counted_by_class(site: Site) -> None:
    site.seen("/", {"User-Agent": "GPTBot"})
    text = site.metrics()
    assert 'client_class="ai_crawler"' in text
    assert 'status="2xx",client_class="default"}' in text


def test_a_changed_list_is_read_again(site: Site) -> None:
    (site.base / "lists" / "ai.txt").write_text("CCBot\n")
    assert eventually(lambda: site.seen("/", {"User-Agent": "CCBot"})["class"] == "ai_crawler")
    assert site.seen("/", {"User-Agent": "GPTBot"})["class"] == "default"
    assert "client_class.list_reloaded" in site.daemon.events()


def test_a_reload_takes_new_classes(site: Site) -> None:
    config = site.base / "muru.toml"
    config.write_text(
        config.read_text() + "\n[[client_class]]\nname = 'late'\nheaders = { 'X-Late' = '.' }\n"
    )
    assert "changed" in site.daemon.muructl("daemon", "reload").stdout
    assert site.seen("/", {"X-Late": "yes"})["class"] == "late"


def test_apply_refuses_limits_the_daemon_does_not_define(site: Site) -> None:
    config = site.base / "bad.toml"
    config.write_text(
        f"""[host.other]
names = ['other.test']
listeners = ['127.0.0.1:{site.daemon.port}']
rate_limits = ['missing']

[[route]]
host = 'other'
path = '/'
app = 'clients'
methods = {{ deny = ['delete'] }}
"""
    )
    completed = site.daemon.muructl("apply", "-f", str(config), check=False)
    assert completed.returncode != 0
    assert "no rate limit named missing" in completed.stdout
    assert "not a method" in completed.stdout

    # Classes and limits are the daemon's, and an applied file's are left
    # out with a warning.

    tables = site.base / "tables.toml"
    tables.write_text("[[client_class]]\nname = 'elsewhere'\nuser_agent = ['x']\n")
    applied = site.daemon.muructl("apply", "-f", str(tables))
    assert "[[client_class]] and [rate_limit] tables belong in the daemon" in (
        applied.stdout + applied.stderr
    )
