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

from support.daemon import SHOP, start_daemon


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def archives(directory: Path, name: str) -> list[Path]:
    return sorted(path for path in directory.glob(f"{name}.*") if path.name != name)


def test_logs_are_rotated_by_size_compressed_and_pruned(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        config_extra="[logs]\nsize = '4KB'\nkeep = 2\ncompress = 'gzip'\nevery = 'off'\n",
    )
    logs = tmp_path / "logs"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        # Each request adds an access log line and a release log line;
        # enough of them pass the size and the daemon rotates on its tick.

        def rotated() -> bool:
            for _ in range(60):
                assert daemon.get("/catalogue").status == 200

            return len(archives(logs, "access.log")) >= 1

        wait_for(rotated)
        wait_for(lambda: any(path.suffix == ".gz" for path in archives(logs, "access.log")))
        events = daemon.muructl("events").stdout
        assert "logs.rotated" in events and "access.log." in events
        archive = next(path for path in archives(logs, "access.log") if path.suffix == ".gz")
        lines = gzip.decompress(archive.read_bytes()).decode().splitlines()
        assert lines and json.loads(lines[0])["app"] == "shop"
        assert archive.name.count(".") == 3, "access.log.YYYYMMDD-HHMMSS.gz"

        # The writers went on into fresh files, which the CLI follows.

        assert daemon.get("/catalogue").status == 200
        wait_for(
            lambda: (logs / "access.log").exists() and (logs / "access.log").stat().st_size > 0
        )
        assert "/catalogue" in daemon.muructl("logs", "--access", "--lines", "5").stdout
        wait_for(lambda: len(archives(logs / "apps" / "shop", "r1.log")) >= 1)
        wait_for(lambda: "shop: served" in daemon.muructl("logs", "shop", "--lines", "20").stdout)

        # Only the newest archives are kept.

        def pruned() -> bool:
            for _ in range(60):
                daemon.get("/catalogue")

            kept = [path for path in archives(logs, "access.log") if path.suffix == ".gz"]
            return len(kept) >= 2 and len(archives(logs, "access.log")) <= 3

        wait_for(pruned, timeout=60)
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
