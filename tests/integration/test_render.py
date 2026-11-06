"""`muructl render` (design specification section 10.4): the release a
command line describes, written as a file for `muructl apply`, with no
daemon asked. Applied with --hold the release starts and traffic stays
where it is, and `muructl deploy --dry-run` says what a deploy would do.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path

from support import server as support
from support.daemon import FACTORY, SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def render(*arguments: str, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    muructl = support.muructl_path()

    if muructl is None:
        support.unavailable("muructl is not built (cargo build -p muructl)")

    return subprocess.run(
        [str(muructl), "render", *arguments],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
        cwd=cwd,
    )


def rendered(*arguments: str, cwd: Path | None = None) -> dict[str, object]:
    completed = render(*arguments, cwd=cwd)
    assert completed.returncode == 0, completed.stderr
    document: dict[str, object] = tomllib.loads(completed.stdout)
    return document


def comment(printed: str, key: str) -> str:
    """Where `--full` says a setting came from."""

    lines = [line for line in printed.splitlines() if line.startswith(f"{key} = ")]
    assert len(lines) == 1, printed
    return lines[0].split("# ", 1)[1]


def test_a_command_line_is_written_as_the_release_it_describes(tmp_path: Path) -> None:
    completed = render(
        "shop",
        str(SHOP),
        "--release",
        "2026-11-06",
        "--processes",
        "4",
        "--max-lifetime",
        "2h",
        "--timeout",
        "soft=30s",
        "--memory",
        "soft_limit=800MB",
        "--env",
        "MODE=production",
        "--secret",
        "TOKEN=vault:shop/token:file",
        "--static",
        "/files/=shop",
    )
    assert completed.returncode == 0, completed.stderr
    printed = completed.stdout
    assert printed.startswith(
        "# The release this command line describes, in the daemon's format.\n"
        "# Save it as muru.daemon.toml and give it to the daemon with:\n"
        "#   muructl apply -f muru.daemon.toml\n"
    ), printed
    assert f"[tool.muru] in {SHOP.resolve() / 'pyproject.toml'}" in printed
    assert "name this one with --release" not in printed

    # The flags come before the source's settings, and what neither gives
    # is left at its default and not written, the interpreter included.

    document = tomllib.loads(printed)
    assert list(document) == ["app"]
    assert document["app"]["shop"]["release"]["2026-11-06"] == {
        "source": str(SHOP),
        "entrypoint": "shop.wsgi:application",
        "processes": 4,
        "threads": 3,
        "hooks": "shop.hooks",
        "max_lifetime": "7200s",
        "timeouts": {"soft_timeout": "30s"},
        "memory": {"soft_limit": "800MB"},
        "env": {
            "MODE": "production",
            "TOKEN": {"secret": {"store": "vault", "key": "shop/token"}, "as": "file"},
        },
        "static": [{"path": "/files/", "dir": "shop"}],
    }

    # With no release named it is r1, and with no source the current
    # directory, in full; a source given is written as given.

    plain = render("shop", cwd=SHOP)
    assert "# The release is written as r1." in plain.stdout
    release = tomllib.loads(plain.stdout)["app"]["shop"]["release"]["r1"]
    assert release["source"] == str(SHOP.resolve())
    relative = render("shop", ".", cwd=SHOP)
    assert "# source is a relative path" in relative.stdout
    assert tomllib.loads(relative.stdout)["app"]["shop"]["release"]["r1"]["source"] == "."

    # The same as JSON, and a project whose settings are a muru.toml.

    as_json = json.loads(render("shop", str(SHOP), "--json").stdout)
    assert as_json["app"]["shop"]["release"]["r1"]["threads"] == 3
    factory = rendered("factory-site", str(FACTORY), "--interface", "wsgi", "--python", "3.13")
    assert factory["app"]["factory-site"]["release"]["r1"] == {  # type: ignore[index]
        "source": str(FACTORY),
        "entrypoint": "factory_site:create_app",
        "interface": "wsgi",
        "factory": True,
        "python": "3.13",
    }


def test_full_writes_every_setting_with_where_it_came_from() -> None:
    completed = render("shop", str(SHOP), "--full", "--threads", "5", "--timeout", "hard=45s")
    assert completed.returncode == 0, completed.stderr
    printed = completed.stdout
    assert "# Every setting is written, with where it came from." in printed
    assert comment(printed, "entrypoint") == "[tool.muru]"
    assert comment(printed, "processes") == "[tool.muru]"
    assert comment(printed, "threads") == "command line"
    assert comment(printed, "sync_threads") == "default"
    assert comment(printed, "hard_timeout") == "command line"
    assert comment(printed, "shutdown_timeout") == "default"
    assert (
        "# Not set, and off unless set: timeouts.soft_timeout, memory.soft_limit, "
        "memory.hard_limit, max_requests, max_lifetime." in printed
    )
    assert "# python is not set: the daemon finds the interpreter" in printed

    release = tomllib.loads(printed)["app"]["shop"]["release"]["r1"]
    assert (release["processes"], release["threads"], release["sync_threads"]) == (2, 5, 8)
    assert release["concurrency"] == 1000 and release["preload"] == "auto"
    assert release["interface"] == "auto" and release["bytecode"] == "redirect"
    assert release["build"] == {"when": "auto"}
    assert release["timeouts"] == {
        "hard_timeout": "45s",
        "hard_drain_grace": "10s",
        "shutdown_timeout": "5s",
        "websocket_drain_grace": "0ms",
        "loop_stall_timeout": "10s",
        "gil_stall_timeout": "30s",
    }
    assert release["memory"] == {
        "measure": "uss",
        "soft_for": "120s",
        "reclaim_first": True,
        "check_interval": "5s",
    }
    assert "python" not in release

    # A server of a profile is written from its flags alone.

    server = render(
        "legacy",
        str(SHOP),
        "--server",
        "gunicorn",
        "--instances",
        "2",
        "--full",
        "--",
        "gunicorn",
        "shop.wsgi",
    )
    assert server.returncode == 0, server.stderr
    assert tomllib.loads(server.stdout)["app"]["legacy"]["release"]["r1"] == {
        "kind": "http",
        "profile": "gunicorn",
        "command": ["gunicorn", "shop.wsgi"],
        "cwd": str(SHOP),
        "bind": "unix",
        "instances": 2,
        "readiness": {"timeout": "60s"},
        "timeouts": {"shutdown_timeout": "5s"},
    }
    assert comment(server.stdout, "instances") == "command line"
    assert comment(server.stdout, "bind") == "default"


def test_what_a_deploy_would_refuse_is_refused_and_nothing_is_written(tmp_path: Path) -> None:
    for arguments, message in [
        (["shop", str(SHOP), "--timeout", "soft=30s", "--timeout", "hard=31s"], "hard_timeout"),
        (["shop", str(tmp_path)], "has no entrypoint in its [tool.muru] table"),
        (["shop", str(tmp_path / "missing")], "does not exist"),
        (["Shop", str(SHOP)], "is not a valid application name"),
        (["shop", str(SHOP), "--release", "No Good"], "is not a valid release id"),
        (["shop", str(SHOP), "--", "gunicorn"], "a command after -- needs --server PROFILE"),
    ]:
        refused = render(*arguments)
        assert refused.returncode != 0, refused.stdout
        assert message in refused.stderr, refused.stderr
        assert refused.stdout == ""

    # The flags that say how a deploy is carried out, and where a daemon
    # is, are not render's.

    for flag in ["--hold", "--no-wait", "--fresh", "--override-freeze", "--socket=x", "--system"]:
        refused = render("shop", str(SHOP), flag)
        assert refused.returncode == 2, flag
        assert "unexpected argument" in refused.stderr, refused.stderr


def test_a_rendered_release_is_applied_held_and_a_deploy_is_planned(
    daemon: Daemon, tmp_path: Path
) -> None:
    daemon.deploy("shop", SHOP)
    python = ["--python", sys.executable]

    # A rendered release applied with --hold starts beside the one that
    # serves, and the plan and the report both say it is held.

    config = tmp_path / "muru.daemon.toml"
    flags = ["--threads", "1", "--max-requests", "500"]
    config.write_text(render("shop", str(SHOP), "--release", "next", *flags, *python).stdout)
    planned = daemon.muructl("apply", "-f", str(config), "--hold", "--dry-run", check=False)
    assert planned.returncode == 2, planned.stdout + planned.stderr
    assert "deploy          shop@next" in planned.stdout
    assert "held: shop.wsgi:application 2x1" in planned.stdout
    applied = daemon.muructl("apply", "-f", str(config), "--hold")
    assert applied.stdout.splitlines()[1].split() == [
        "deploy",
        "shop@next",
        "ready",
        "and",
        "held:",
        "not",
        "serving",
    ]
    assert daemon.releases("shop") == [
        {"release": "shop@next", "state": "ready", "serving": ""},
        {"release": "shop@r1", "state": "ready", "serving": "yes"},
    ]

    # It is the release a fresh deploy of the same command line gives,
    # and a rollout moves traffic to it.

    shown = json.loads(daemon.muructl("release", "show", "shop@next", "--json").stdout)
    assert (shown["processes"], shown["threads"]) == (2, 1)
    daemon.muructl("rollout", "bluegreen", "shop@next")
    assert daemon.releases("shop")[0] == {
        "release": "shop@next",
        "state": "ready",
        "serving": "yes",
    }

    # A deploy's plan: the id it would take, each setting with its layer,
    # and what becomes of the release that serves. Nothing is changed.

    before = daemon.releases("shop")
    plan = daemon.muructl("deploy", "shop", str(SHOP), "--processes", "3", "--dry-run", *python)
    assert plan.returncode == 0
    printed = plan.stdout
    assert printed.startswith("muru: plan (nothing changed): release shop@r2, "), printed
    assert "processes = 3" in printed and "(deploy flags)" in printed
    assert "threads = 3" in printed and "([tool.muru])" in printed
    assert "max_requests = 500" in printed and "(the previous release)" in printed
    assert "settings nothing else gives come from shop@next; --fresh takes none" in printed
    assert (
        "would take over from shop@next, which would stay warm for 1800s with 1 worker" in printed
    )
    assert daemon.releases("shop") == before

    as_json = json.loads(
        daemon.muructl(
            "deploy", "shop", str(SHOP), "--fresh", "--hold", "--dry-run", "--json", *python
        ).stdout
    )
    assert as_json["release"] == "r2" and as_json["hold"] is True
    assert as_json["serving"] == "next" and as_json["settings_from"] == ""
    layers = {setting["key"]: setting["layer"] for setting in as_json["settings"]}
    assert layers["threads"] == "[tool.muru]" and layers["sync_threads"] == "default"

    # What would refuse the deploy is said, with exit code 1.

    refused = daemon.muructl(
        "deploy", "shop", str(SHOP), "--release", "next", "--dry-run", *python, check=False
    )
    assert refused.returncode == 1
    assert "muru: the deploy would be refused:" in refused.stdout
    assert "release shop@next already exists" in refused.stdout
    bad = daemon.muructl(
        "deploy", "shop", str(SHOP), "--timeout", "soft=30s", "--timeout", "hard=31s",
        "--dry-run", *python, check=False,
    )  # fmt: skip
    assert bad.returncode == 1 and "hard_timeout" in bad.stdout
    assert daemon.releases("shop") == before
