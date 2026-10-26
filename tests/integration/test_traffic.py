"""Traffic policies between releases (design specification section 9.4).

A route's `traffic` table shares its requests between an application's
releases: each client is bucketed by a key (an anonymous id cookie the edge
issues, a header, a cookie or the client address), the newest release's
buckets sit at the top, a sticky cookie keeps a client on the release it
first got, and a preview header or cookie pins a release, even one with no
share. A release a policy names is kept running, and one no policy names
drains, after sticky clients' grace.
"""

from __future__ import annotations

import json
import time
from collections import Counter
from pathlib import Path

from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def policy_file(directory: Path, traffic: str) -> Path:
    """One host, one route to the shop application, and this policy."""

    path = directory / "traffic.toml"
    path.write_text(
        f"""[host.shop]
names = ["shop.test"]
default = true

[[route]]
host = "shop"
app = "shop"
{traffic}"""
    )
    return path


def served_by(daemon: Daemon, **headers: str) -> tuple[str, list[str]]:
    """The release that answered, and the cookies the edge set."""

    response = daemon.request("/", host="shop.test", headers=headers)
    assert response.status == 200, response.body
    release: str = json.loads(response.body)["release"]
    return release, response.header_values("Set-Cookie")


def states(daemon: Daemon) -> dict[str, str]:
    return {row["release"]: row["state"] for row in daemon.releases("shop")}


def test_a_split_shares_clients_and_cookies_pin_them(daemon: Daemon, tmp_path: Path) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)
    assert states(daemon) == {"shop@r1": "retained", "shop@r2": "ready"}

    # Half the buckets go to r1, which the policy brings back from
    # retention; r2 serves the rest, as the serving release.

    applied = daemon.muructl(
        "apply",
        "-f",
        str(
            policy_file(
                tmp_path,
                """
[route.traffic]
split = { r1 = 50 }
sticky = true
sticky_grace = "2s"
""",
            )
        ),
    )
    assert applied.returncode == 0, applied.stdout + applied.stderr
    assert states(daemon) == {"shop@r1": "ready", "shop@r2": "ready"}
    assert "shop (r1 50%, sticky)" in daemon.muructl("route", "list").stdout
    assert "traffic_policy.changed" in daemon.events()

    seen: Counter[str] = Counter()

    for n in range(200):
        release, cookies = served_by(daemon, Cookie=f"muru_id=client-{n}")
        seen[release] += 1
        assert any(cookie.startswith(f"muru_release={release};") for cookie in cookies), (
            release,
            cookies,
        )

    assert set(seen) == {"r1", "r2"} and 50 <= seen["r1"] <= 150, seen

    # A client with no id gets one, and keeps its release with it.

    release, cookies = served_by(daemon)
    issued = [cookie for cookie in cookies if cookie.startswith("muru_id=")]
    assert len(issued) == 1 and "HttpOnly" in issued[0], cookies
    muru_id = issued[0].split(";")[0]

    for _ in range(5):
        assert served_by(daemon, Cookie=muru_id)[0] == release

    # The sticky cookie holds whatever the bucket says, and a preview pins
    # a release for the request alone.

    for n in range(20):
        assert served_by(daemon, Cookie=f"muru_id=client-{n}; muru_release=r1")[0] == "r1"
        assert served_by(daemon, Cookie=f"muru_id=client-{n}; muru_preview=r2")[0] == "r2"
        assert (
            served_by(daemon, **{"Muru-Preview": "r1", "Cookie": f"muru_id=client-{n}"})[0] == "r1"
        )

    # At 0% a release takes only previews and sticky clients.

    daemon.muructl(
        "apply",
        "-f",
        str(
            policy_file(
                tmp_path,
                '[route.traffic]\nsplit = { r1 = 0 }\nsticky = true\nsticky_grace = "2s"\n',
            )
        ),
    )
    assert all(served_by(daemon, Cookie=f"muru_id=fresh-{n}")[0] == "r2" for n in range(50))
    assert served_by(daemon, Cookie="muru_release=r1")[0] == "r1"
    assert served_by(daemon, **{"Muru-Preview": "r1"})[0] == "r1"

    # With no share left, r1 drains once its sticky clients' grace passes;
    # until then their cookie still reaches it, afterwards it is ignored.

    daemon.muructl(
        "apply",
        "-f",
        str(policy_file(tmp_path, '[route.traffic]\nsticky = true\nsticky_grace = "2s"\n')),
    )
    assert states(daemon)["shop@r1"] == "draining"
    assert served_by(daemon, Cookie="muru_release=r1")[0] == "r1"
    deadline = time.monotonic() + 15

    while states(daemon)["shop@r1"] != "retained" and time.monotonic() < deadline:
        time.sleep(0.5)

    assert states(daemon) == {"shop@r1": "retained", "shop@r2": "ready"}
    assert served_by(daemon, Cookie="muru_release=r1")[0] == "r2"


def test_header_and_client_keys_and_invalid_policies(daemon: Daemon, tmp_path: Path) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)
    daemon.muructl(
        "apply",
        "-f",
        str(
            policy_file(
                tmp_path, '[route.traffic]\nsplit = { r1 = 50 }\nhash_key = "header:x-user"\n'
            )
        ),
    )
    assert "by header:x-user" in daemon.muructl("route", "list").stdout

    # The same user lands on the same release every time, without any
    # cookie; users spread over both releases.

    by_user = {f"user-{n}": served_by(daemon, **{"X-User": f"user-{n}"}) for n in range(40)}
    assert all(cookies == [] for _, cookies in by_user.values())
    assert {release for release, _ in by_user.values()} == {"r1", "r2"}
    assert all(
        served_by(daemon, **{"X-User": user})[0] == release
        for user, (release, _) in by_user.items()
    )

    # Without the header the client address is the key, so every request
    # from this test lands on one release.

    assert len({served_by(daemon)[0] for _ in range(10)}) == 1

    for policy, complaint in [
        ("[route.traffic]\nsplit = { r9 = 10 }\n", "names shop@r9, which does not exist"),
        ("[route.traffic]\nsplit = { r1 = 60, r2 = 50 }\n", "more than 100 percent"),
        (
            '[route.traffic]\nhash_key = "ip"\n',
            "hash_key must be cookie, header:NAME, cookie:NAME or client",
        ),
        ('[route.traffic]\nsticky_grace = "soon"\n', "sticky_grace"),
    ]:
        refused = daemon.muructl("apply", "-f", str(policy_file(tmp_path, policy)), check=False)
        assert refused.returncode != 0 and complaint in refused.stdout + refused.stderr, (
            refused.stdout + refused.stderr
        )


def test_a_cohort_and_a_route_take_releases_of_their_own(daemon: Daemon, tmp_path: Path) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)
    config = tmp_path / "cohorts.toml"
    config.write_text(
        """[[client_class]]
name = 'beta'
cookies = { plan = '^beta$' }

[host.shop]
names = ["shop.test"]
default = true

[[route]]
host = "shop"
app = "shop"
traffic = { classes = { beta = 'r1' } }

[[route]]
host = "shop"
path = "/legacy"
app = "shop"
traffic = { split = { r1 = 100 } }
"""
    )
    applied = daemon.muructl("apply", "-f", str(config))
    assert applied.returncode == 0, applied.stdout + applied.stderr

    # The beta cohort, known by its cookie, gets r1, and everyone else the
    # serving release.

    assert {served_by(daemon, Cookie="plan=beta")[0] for _ in range(5)} == {"r1"}
    others = {served_by(daemon, Cookie=f"plan=free; muru_id=c{n}")[0] for n in range(20)}
    assert others == {"r2"}

    # One route of the application stays on r1 for everyone.

    for n in range(10):
        response = daemon.request("/legacy", host="shop.test", headers={"Cookie": f"muru_id=l{n}"})
        assert json.loads(response.body)["release"] == "r1"
