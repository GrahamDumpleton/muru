"""Declarative configuration (design specification section 10.4).

`muructl apply -f` deploys the releases a file declares, changes nothing
when applied again, plans without acting under --dry-run, prunes what the
file no longer contains, and refuses a file with errors without applying
any of it. `config export` writes desired state as a file that applies
back without changes, and `config diff` reports drift either way.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time
import tomllib
from pathlib import Path

from support import server as support
from support.daemon import FACTORY, SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture

SHOP_SOURCE = os.path.realpath(SHOP)


def release(app: str, release_id: str, source: Path | str = SHOP, **settings: object) -> str:
    """One `[app.NAME.release.ID]` table, with this interpreter."""

    lines = [
        f"[app.{app}.release.{release_id}]",
        f"source = '{source}'",
        f"python = '{sys.executable}'",
    ]

    for key, value in settings.items():
        written = f"'{value}'" if isinstance(value, str) else value
        lines.append(f"{key} = {written}")

    return "\n".join(lines) + "\n\n"


def apply(daemon: Daemon, path: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    return daemon.muructl("apply", "-f", str(path), *arguments, check=False)


def test_an_interpreter_relative_to_the_file_is_resolved_there(
    daemon: Daemon, tmp_path: Path
) -> None:
    config = tmp_path / "shop.toml"
    relative = os.path.relpath(sys.executable, tmp_path)
    config.write_text(release("shop", "r1").replace(sys.executable, relative))
    assert relative in config.read_text() and os.sep in relative
    assert apply(daemon, config).returncode == 0
    inspected = daemon.muructl("debug", "inspect", "shop").stdout
    assert f"at {os.path.normpath(tmp_path / relative)}," in inspected, inspected


def test_apply_deploys_declared_releases_and_is_idempotent(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "shop.toml"
    config.write_text(release("shop", "r1"))

    first = apply(daemon, config)
    assert first.returncode == 0, first.stdout + first.stderr
    lines = first.stdout.splitlines()
    assert lines[0] == "muru: applied"
    assert lines[1].split() == ["create-app", "shop", "created"]
    assert lines[2].split() == ["deploy", "shop@r1", "ready", "and", "serving"]
    assert daemon.get("/").status == 200
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]
    assert "config.applied" in daemon.events()

    # The same file again changes nothing.

    again = apply(daemon, config)
    assert again.returncode == 0, again.stdout + again.stderr
    assert again.stdout.strip() == "muru: no changes (1 object as declared)"
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]


def test_dry_run_plans_without_changing_anything(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "shop.toml"
    config.write_text(release("shop", "r1"))
    apply(daemon, config)
    config.write_text(release("shop", "r1") + release("shop", "r2", threads=1))

    planned = apply(daemon, config, "--dry-run")
    assert planned.returncode == 2, planned.stdout + planned.stderr
    lines = planned.stdout.splitlines()
    assert lines[0] == "muru: plan (nothing changed)"
    assert lines[1].split()[:2] == ["deploy", "shop@r2"]
    assert "shop.wsgi:application 2x1" in lines[1]
    assert lines[2].strip() == "1 object as declared"
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]

    # The JSON report says the same.

    report = json.loads(apply(daemon, config, "--dry-run", "--json").stdout)
    assert report["errors"] == []
    assert report["applied"] is False
    assert report["unchanged"] == 1
    assert [(change["action"], change["subject"]) for change in report["changes"]] == [
        ("deploy", "shop@r2")
    ]

    # Applying for real moves traffic to r2 and keeps r1 warm.

    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr
    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "ready", "serving": "yes"},
        {"release": "shop@r1", "state": "retained", "serving": ""},
    ]


def test_prune_removes_what_the_files_no_longer_contain(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "shop.toml"
    config.write_text(release("shop", "r1") + release("shop", "r2"))
    apply(daemon, config)

    # Without --prune, a release missing from the file is left alone.

    config.write_text(release("shop", "r2"))
    kept = apply(daemon, config)
    assert kept.stdout.strip() == "muru: no changes (1 object as declared)"
    assert [row["release"] for row in daemon.releases("shop")] == ["shop@r2", "shop@r1"]

    pruned = apply(daemon, config, "--prune")
    assert pruned.returncode == 0, pruned.stdout + pruned.stderr
    assert pruned.stdout.splitlines()[1].split() == [
        "remove-release",
        "shop@r1",
        "stopped",
        "and",
        "removed",
    ]
    assert daemon.releases("shop") == [{"release": "shop@r2", "state": "ready", "serving": "yes"}]

    # An empty file with --prune removes the application itself.

    config.write_text("# nothing declared\n")
    emptied = apply(daemon, config, "--prune")
    assert emptied.returncode == 0, emptied.stdout + emptied.stderr
    actions = [line.split()[:2] for line in emptied.stdout.splitlines()[1:]]
    assert actions == [["remove-release", "shop@r2"], ["remove-app", "shop"]]
    assert daemon.muructl("app", "list").stdout.strip() == "no applications"
    assert daemon.get("/").status == 503


def test_export_applies_back_without_changes(daemon: Daemon, tmp_path: Path) -> None:
    daemon.deploy("shop", SHOP)

    exported = daemon.muructl("config", "export").stdout
    assert exported.startswith("# Muru desired state: muructl ")
    document = tomllib.loads(exported)
    r1 = document["app"]["shop"]["release"]["r1"]
    assert r1["source"] == SHOP_SOURCE
    assert r1["entrypoint"] == "shop.wsgi:application"
    assert r1["python"] == sys.executable
    assert (r1["processes"], r1["threads"], r1["hooks"]) == (2, 3, "shop.hooks")
    assert (r1["interface"], r1["preload"]) == ("auto", "auto")

    config = tmp_path / "export.toml"
    config.write_text(exported)
    assert apply(daemon, config).stdout.strip() == "muru: no changes (1 object as declared)"
    assert daemon.muructl("config", "diff", "-f", str(config), check=False).returncode == 0

    # By default only the serving release is written; --all-releases
    # writes every release, and --json the same document as JSON.

    daemon.deploy("shop", SHOP, "--threads", "1")
    current = tomllib.loads(daemon.muructl("config", "export").stdout)
    assert list(current["app"]["shop"]["release"]) == ["r2"]
    every = tomllib.loads(daemon.muructl("config", "export", "--all-releases").stdout)
    assert list(every["app"]["shop"]["release"]) == ["r1", "r2"]
    as_json = json.loads(daemon.muructl("config", "export", "--json").stdout)
    assert as_json["app"]["shop"]["release"]["r2"]["threads"] == 1


def test_diff_reports_drift_in_both_directions(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "shop.toml"
    config.write_text(release("shop", "r1"))
    apply(daemon, config)

    # A deploy made by hand is not in the file; a release in the file is
    # not in the daemon.

    daemon.deploy("shop", SHOP)
    config.write_text(release("shop", "r1") + release("shop", "r3"))
    diff = daemon.muructl("config", "diff", "-f", str(config), check=False)
    assert diff.returncode == 2, diff.stdout + diff.stderr
    actions = [line.split()[:2] for line in diff.stdout.splitlines()[1:3]]
    assert actions == [["deploy", "shop@r3"], ["remove-release", "shop@r2"]]
    assert daemon.releases("shop")[0]["release"] == "shop@r2", "a diff changes nothing"


def test_a_file_with_errors_applies_nothing(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "shop.toml"

    # An unknown key is reported with its line.

    config.write_text(release("shop", "r1") + "[app.shop.release.r2]\nentry_point = 'a:b'\n")
    unknown = apply(daemon, config)
    assert unknown.returncode == 1
    assert "entry_point" in unknown.stderr, unknown.stdout + unknown.stderr
    assert daemon.releases("shop") == []

    # A release without a source, a bad name and a changed release are
    # all named, and the valid release beside them is not deployed.

    config.write_text(release("shop", "r1"))
    apply(daemon, config)
    config.write_text(
        release("shop", "r1", threads=1)
        + "[app.shop.release.r2]\nentrypoint = 'a:b'\n\n"
        + release("Blog", "r1")
        + release("blog", "r1")
    )
    invalid = apply(daemon, config)
    assert invalid.returncode == 1, invalid.stdout + invalid.stderr
    lines = invalid.stdout.splitlines()
    assert lines[0] == "muru: nothing applied; the configuration has errors:"
    errors = "\n".join(lines[1:])
    assert '"Blog" is not a valid application name' in errors
    assert "shop@r1 already exists with different settings (threads 1, recorded 3)" in errors
    assert "shop@r2 has no source directory" in errors
    assert "blog" not in daemon.muructl("app", "list").stdout
    assert daemon.releases() == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]


def test_a_directory_of_files_with_relative_sources(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "config"
    config.mkdir()
    relative = os.path.relpath(SHOP, config)
    (config / "10-shop.toml").write_text(release("shop", "r1", source=relative))
    (config / "20-factory.toml").write_text(
        release("factory", "r1", source=FACTORY, interface="wsgi")
    )
    (config / "notes.txt").write_text("not configuration\n")

    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr
    assert sorted(row["release"] for row in daemon.releases()) == ["factory@r1", "shop@r1"]
    assert all(row["state"] == "ready" for row in daemon.releases())
    exported = tomllib.loads(daemon.muructl("config", "export", "--app", "shop").stdout)
    assert exported["app"]["shop"]["release"]["r1"]["source"] == SHOP_SOURCE
    assert "factory" not in exported["app"]

    # Listeners and limits in an applied file are reported, not applied.

    (config / "30-node.toml").write_text("[[listener]]\naddress = '127.0.0.1:9999'\n")
    warned = apply(daemon, config)
    assert warned.returncode == 0, warned.stdout + warned.stderr
    assert "warning: [[listener]] entries belong in the daemon configuration file" in warned.stdout


def test_recycling_and_timeouts_round_trip_through_export(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "shop.toml"
    config.write_text(
        release("shop", "r1", max_requests=250, max_lifetime="1h")
        + "[app.shop.release.r1.timeouts]\nsoft_timeout = '20s'\nhard_timeout = '30s'\n"
    )
    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr

    exported = tomllib.loads(daemon.muructl("config", "export").stdout)
    written = exported["app"]["shop"]["release"]["r1"]
    assert written["max_requests"] == 250
    assert written["max_lifetime"] == "3600s"
    assert written["timeouts"]["soft_timeout"] == "20s"
    assert written["timeouts"]["hard_timeout"] == "30s"
    assert daemon.muructl("config", "diff", "-f", str(config), check=False).returncode == 0

    # A declared release that differs on these is immutable like any other.

    config.write_text(release("shop", "r1", max_requests=300))
    differing = apply(daemon, config)
    assert differing.returncode == 1
    assert "max_requests 300, recorded 250" in differing.stdout + differing.stderr


def test_a_memory_table_round_trips_through_export(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "shop.toml"
    config.write_text(
        release("shop", "r1")
        + "[app.shop.release.r1.memory]\nmeasure = 'rss'\n"
        + "hard_limit = '1GB'\nsoft_limit = '800MB'\n"
    )
    applied = apply(daemon, config)
    assert applied.returncode == 0, applied.stdout + applied.stderr

    exported = tomllib.loads(daemon.muructl("config", "export").stdout)
    memory = exported["app"]["shop"]["release"]["r1"]["memory"]
    assert memory == {
        "measure": "rss",
        "soft_limit": "800MB",
        "soft_for": "120s",
        "reclaim_first": True,
        "hard_limit": "1GB",
        "check_interval": "5s",
    }
    assert daemon.muructl("config", "diff", "-f", str(config), check=False).returncode == 0


def test_relative_paths_are_taken_from_the_file(daemon: Daemon) -> None:
    """Every path in an applied file is resolved from the file's directory,
    as `source` is, since the daemon runs elsewhere (here, the test's own
    working directory): static directories, file hint roots, a host's and a
    route's error page files, and an overlay's static target."""

    site = daemon.base / "site"

    for directory in ("static", "more", "snapshot", "reports", "pages"):
        (site / directory).mkdir(parents=True)

    (site / "static" / "hello.txt").write_text("hello")
    (site / "pages" / "route-404.html").write_text("route page")
    (site / "pages" / "host-404.html").write_text("host page")
    (site / "site.toml").write_text(
        f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1
sendfile_roots = ['reports']

[host.site]
names = ['site.test']
default = true

[host.site.error_pages]
404 = {{ file = 'pages/host-404.html' }}

[[route]]
name = 'files'
host = 'site'
path = '/files/'
static = 'static'
error_pages = {{ 404 = {{ file = 'pages/route-404.html' }} }}

[[route]]
host = 'site'
path = '/more/'
static = 'more'

[overlay.snapshot]
routes.files.target = {{ static = 'snapshot' }}
"""
    )
    applied = daemon.muructl("apply", "-f", str(site / "site.toml"), check=False)
    assert applied.returncode == 0, applied.stdout + applied.stderr

    # The exported configuration carries the resolved paths.

    exported = daemon.muructl("config", "export").stdout

    for path in ("static", "snapshot", "reports", "pages/route-404.html", "pages/host-404.html"):
        assert str(site / path) in exported, (path, exported)

    # And the files are found: the static route serves its directory, and
    # a miss gets the nearest page, the route's or the host's.

    assert daemon.request("/files/hello.txt", host="site.test").body == b"hello"
    missed = daemon.request("/files/missing", host="site.test")
    assert (missed.status, missed.body) == (404, b"route page")
    missed = daemon.request("/more/missing", host="site.test")
    assert (missed.status, missed.body) == (404, b"host page")


def test_a_file_named_by_a_relative_path_gives_absolute_paths(daemon: Daemon) -> None:
    """The paths resolved from an applied file's directory come out absolute
    whether `-f` named the file by an absolute or a relative path, since
    the daemon checks some of them, `sendfile_roots` among them, for being
    absolute."""

    site = daemon.base / "relative"
    (site / "reports").mkdir(parents=True)
    (site / "site.toml").write_text(
        f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1
sendfile_roots = ['reports']
"""
    )
    muructl = support.muructl_path()
    assert muructl is not None
    applied = subprocess.run(
        [str(muructl), "apply", "-f", "site.toml", "--socket", str(daemon.run_dir / "admin.sock")],
        cwd=site,
        capture_output=True,
        text=True,
        check=False,
    )
    assert applied.returncode == 0, applied.stdout + applied.stderr
    exported = daemon.muructl("config", "export").stdout
    assert str(site / "reports") in exported, exported


def test_a_frozen_worker_does_not_hold_up_other_applications(daemon: Daemon) -> None:
    """One worker that accepts the edge's connection and never answers
    HELLO (a stopped process) is left out of its pool, and the snapshot is
    applied without it, so a routing change for another application still
    lands. The daemon records the worker as unreachable and replaces it."""

    assert daemon.deploy("shop", SHOP, "--processes", "2") == (
        "muru: release shop@r1 is ready and serving"
    )
    assert daemon.deploy("other", SHOP, "--processes", "1") == (
        "muru: release other@r1 is ready and serving"
    )
    status = json.loads(daemon.muructl("debug", "status", "shop", "--json").stdout)
    pids = {worker["pid"] for worker in status["workers"]}
    assert len(pids) == 2, status
    frozen = pids.pop()
    os.kill(frozen, signal.SIGSTOP)

    try:
        site = daemon.base / "frozen"
        site.mkdir()
        (site / "site.toml").write_text(
            """[host.shop]
names = ['shop.test']
default = true

[host.other]
names = ['other.test']

[[route]]
host = 'shop'
app = 'shop'

[[route]]
host = 'other'
app = 'other'
"""
        )
        started = time.monotonic()
        applied = daemon.muructl("apply", "-f", str(site / "site.toml"), check=False)
        assert applied.returncode == 0, applied.stdout + applied.stderr
        assert time.monotonic() - started < 15

        # The change landed, the other worker of the first application still
        # serves, and the frozen one is named and replaced.

        assert json.loads(daemon.request("/", host="other.test").body)["app"] == "other"
        served = daemon.request("/")
        assert served.status == 200, served.body
        assert json.loads(served.body)["pid"] != frozen
        log = daemon.muructl("events").stdout
        assert "worker.unreachable" in daemon.events(), log
        assert "did not answer the edge" in log and "it is being replaced" in log, log

        for _ in range(50):
            if "worker.restarted" in daemon.events():
                break

            time.sleep(0.2)
        else:
            raise AssertionError(daemon.muructl("events").stdout)
    finally:
        try:
            os.kill(frozen, signal.SIGCONT)
        except ProcessLookupError:
            pass
