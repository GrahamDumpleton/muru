"""The muru.uploads module (design specification section 4.11), over real
files standing in for what the edge passes: what applications see
outside Muru, reading, saving by link or copy, and overwriting.
"""

from __future__ import annotations

import io
import os
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

from muru import uploads


@pytest.fixture
def stored(tmp_path: Path) -> Iterator[dict[str, Any]]:
    """A description as the worker gives it, over an open file."""

    spool = tmp_path / "spool"
    spool.mkdir()
    path = spool / "1-0"
    path.write_bytes(b"0123456789")
    fd = os.open(path, os.O_RDONLY)

    try:
        yield {
            "ref": "1",
            "field": "avatar",
            "name": "a.txt",
            "content_type": "text/plain",
            "size": 10,
            "digests": {"sha256": "abc"},
            "fd": fd,
            "path": None,
        }
    finally:
        os.close(fd)


def test_outside_muru_there_are_no_uploads() -> None:
    assert uploads.files({}) == []
    assert uploads.get({}, "1") is None
    assert not uploads.is_precheck({})
    assert uploads.files({"type": "http", "extensions": {}}) == []
    assert not uploads.is_precheck({"type": "http"})


def test_uploads_are_found_in_an_environ_and_a_scope(stored: dict[str, Any]) -> None:
    environ = {"muru.uploads": [stored], "muru.precheck": True}
    scope = {"type": "http", "extensions": {"muru": {"uploads": [stored], "precheck": True}}}

    for request in (environ, scope):
        (upload,) = uploads.files(request)
        assert (upload.field, upload.name, upload.size, upload.ref) == ("avatar", "a.txt", 10, "1")
        assert upload.sha256 == "abc"
        assert uploads.get(request, b"1") is not None
        assert uploads.get(request, "2") is None
        assert uploads.is_precheck(request)


def test_open_gives_independent_seekable_files(stored: dict[str, Any]) -> None:
    upload = uploads.Upload(stored)

    with upload.open() as first, upload.open() as second:
        assert first.read(4) == b"0123"
        assert second.read() == b"0123456789"
        assert first.read() == b"456789"
        first.seek(-3, io.SEEK_END)
        assert first.read() == b"789"
        assert first.seekable() and first.readable()

    assert first.closed


def test_path_opens_the_file(stored: dict[str, Any]) -> None:
    with open(uploads.Upload(stored).path, "rb") as handle:
        handle.seek(0)
        assert handle.read() == b"0123456789"


def test_save_refuses_an_existing_file_unless_told_to_overwrite(
    stored: dict[str, Any], tmp_path: Path
) -> None:
    upload = uploads.Upload(stored)
    target = tmp_path / "saved.bin"
    assert upload.save(target) == str(target)
    assert target.read_bytes() == b"0123456789"

    with pytest.raises(FileExistsError):
        upload.save(target)

    # A new file in its place: where save linked, writing to the target
    # would write to the upload itself.

    target.unlink()
    target.write_bytes(b"old")
    assert upload.save(target, overwrite=True) == str(target)
    assert target.read_bytes() == b"0123456789"
    assert [path.name for path in tmp_path.iterdir() if path.name.endswith(".tmp")] == []


def test_save_links_a_named_spool_file(stored: dict[str, Any], tmp_path: Path) -> None:
    stored["path"] = str(tmp_path / "spool" / "1-0")
    upload = uploads.Upload(stored)
    target = tmp_path / "linked.bin"
    upload.save(target)
    assert target.stat().st_ino == os.fstat(stored["fd"]).st_ino

    replaced = tmp_path / "replaced.bin"
    replaced.write_bytes(b"old")
    upload.save(replaced, overwrite=True)
    assert replaced.stat().st_ino == os.fstat(stored["fd"]).st_ino


@pytest.mark.skipif(not sys.platform.startswith("linux"), reason="/proc/self/fd is Linux's")
def test_save_links_through_proc_on_linux(stored: dict[str, Any], tmp_path: Path) -> None:
    target = tmp_path / "linked.bin"
    uploads.Upload(stored).save(target)
    assert target.stat().st_ino == os.fstat(stored["fd"]).st_ino
