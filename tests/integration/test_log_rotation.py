"""Built-in log rotation (design specification section 11.3): the daemon
renames a log that reaches its size, has the writers reopen their files,
compresses the archive in the background and keeps a bounded number of
archives; external rotation turns it off."""

from __future__ import annotations

import gzip
import json
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import SHOP, start_daemon


def wait_for(
    predicate: Callable[[], bool],
    what: str,
    timeout: float = 30.0,
    describe: Callable[[], str] = lambda: "",
) -> None:
    """Wait for the predicate, failing with what was awaited and `describe()`."""

    deadline = time.monotonic() + timeout

    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {what}\n{describe()}")

        time.sleep(0.1)


def listing(directory: Path) -> str:
    """Every file under the directory with its size, one a line."""

    return "\n".join(
        f"{path.relative_to(directory)} {path.stat().st_size}"
        for path in sorted(directory.rglob("*"))
        if path.is_file()
    )


def archives(directory: Path, name: str) -> list[Path]:
    return sorted(path for path in directory.glob(f"{name}.*") if path.name != name)


def test_a_rotation_policy_added_by_reload_takes_effect(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, listeners=[(support.free_port(), False)])
    logs = tmp_path / "logs"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        for _ in range(60):
            assert daemon.get("/catalogue").status == 200

        time.sleep(1.5)
        assert archives(logs, "access.log") == []

        with (daemon.base / "muru.toml").open("a") as handle:
            handle.write("\n[logs]\nsize = '4KB'\nkeep = 2\ncompress = 'off'\nevery = 'off'\n")

        reloaded = daemon.muructl("daemon", "reload").stdout
        assert "changed [logs] rotation (the daemon rotates on the new policy)" in reloaded, (
            reloaded
        )
        wait_for(lambda: len(archives(logs, "access.log")) >= 1, "an access log archive")
    finally:
        daemon.stop()


def test_logs_are_rotated_by_size_compressed_and_pruned(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        config_extra="[logs]\nsize = '4KB'\nkeep = 2\ncompress = 'gzip'\nevery = 'off'\n",
    )
    logs = tmp_path / "logs"

    def state() -> str:
        return f"{listing(logs)}\n{daemon.muructl('events').stdout}"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        # Each request adds an access log line and a release log line;
        # enough of them pass the size and the daemon rotates on its tick.

        def rotated(directory: Path, name: str) -> Callable[[], bool]:
            def check() -> bool:
                for _ in range(60):
                    assert daemon.get("/catalogue").status == 200

                return len(archives(directory, name)) >= 1

            return check

        wait_for(rotated(logs, "access.log"), "an access log archive", describe=state)
        wait_for(
            lambda: any(path.suffix == ".gz" for path in archives(logs, "access.log")),
            "the access log archive to be compressed",
            describe=state,
        )
        events = daemon.muructl("events").stdout
        assert "logs.rotated" in events and "access.log." in events
        archive = next(path for path in archives(logs, "access.log") if path.suffix == ".gz")
        lines = gzip.decompress(archive.read_bytes()).decode().splitlines()
        assert lines and json.loads(lines[0])["app"] == "shop"
        assert archive.name.count(".") == 3, "access.log.YYYYMMDD-HHMMSS.gz"

        # The writers went on into fresh files, which the CLI follows.

        assert daemon.get("/catalogue").status == 200
        wait_for(
            lambda: (logs / "access.log").exists() and (logs / "access.log").stat().st_size > 0,
            "a fresh access log",
            describe=state,
        )
        assert "/catalogue" in daemon.muructl("logs", "--access", "--lines", "5").stdout

        # A release log line is shorter than an access log line, so the
        # release log can still be under the size when the access log
        # first rotates; the requests go on until it passes it too.

        wait_for(rotated(logs / "apps" / "shop", "r1.log"), "a release log archive", describe=state)
        wait_for(
            lambda: "shop: served" in daemon.muructl("logs", "shop", "--lines", "20").stdout,
            "the release log to be followed",
            describe=state,
        )

        # Only the newest archives are kept.

        def pruned() -> bool:
            for _ in range(60):
                daemon.get("/catalogue")

            kept = [path for path in archives(logs, "access.log") if path.suffix == ".gz"]
            return len(kept) >= 2 and len(archives(logs, "access.log")) <= 3

        wait_for(pruned, "the archives to be pruned", timeout=60, describe=state)
        time.sleep(1.0)
        assert len([p for p in archives(logs, "access.log") if p.suffix == ".gz"]) <= 2
    finally:
        daemon.stop()


def test_external_rotation_leaves_the_files_alone(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra="[logs]\nrotate = 'external'\nsize = '1KB'\n")
    logs = tmp_path / "logs"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        for _ in range(40):
            assert daemon.get("/catalogue").status == 200

        time.sleep(2.5)
        assert archives(logs, "access.log") == []
        assert "logs.rotated" not in daemon.muructl("events").stdout
        assert (logs / "access.log").stat().st_size > 1024
    finally:
        daemon.stop()


def test_a_bad_logs_table_is_refused(tmp_path: Path) -> None:
    import pytest

    with pytest.raises(RuntimeError, match="every must be off, hourly, daily or weekly"):
        start_daemon(tmp_path, config_extra="[logs]\nevery = 'fortnightly'\n")
