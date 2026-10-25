"""File hints (design specification sections 4.9, 5.7 and 6.5).

An application hands the body of a response to the edge in two ways. By
descriptor: a `wsgi.file_wrapper` object returned as the iterable, or the
ASGI `pathsend` and `zerocopysend` extensions, pass a duplicate of the
open file on a FILE frame with an offset and length, and the edge sends
exactly that range with the status and headers the application chose
while the application thread is free. By path: an `X-Muru-Sendfile`
header, or the second header a mount's `sendfile_header` names, makes the
edge serve a file inside the mount's `sendfile_roots` with static-file
behaviour, from an application, a trusted upstream or a managed server.
"""

from __future__ import annotations

import hashlib
import http.client
import json
import shutil
import socket
import sys
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support
from support.daemon import start_daemon
from support.upstream import Upstream

FILES = support.ROOT / "tests" / "fixtures" / "projects" / "files"

BIG = bytes(range(256)) * 4096 * 3  # 3 MiB
SMALL = b"a small file of text\n"


def write_files(directory: Path) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "big.bin").write_bytes(BIG)
    (directory / "small.txt").write_bytes(SMALL)
    (directory / "nested").mkdir(exist_ok=True)
    (directory / "nested" / "inner.txt").write_bytes(b"inner\n")


def write_project(project: Path, roots: Path) -> None:
    """Copy the fixture module and write a muru.toml naming `roots`."""

    project.mkdir(parents=True, exist_ok=True)
    shutil.copy(FILES / "files.py", project / "files.py")
    (project / "muru.toml").write_text(
        f"""entrypoint = "files:wsgi_app"
threads = 2
sendfile_roots = ['{roots}']

[mounts.asgi]
path = "/asgi"
entrypoint = "files:asgi_app"

[mounts.compat]
path = "/compat"
entrypoint = "files:compat_app"
interface = "wsgi"
sendfile_header = "X-Sendfile"

[mounts.starlette]
path = "/starlette"
entrypoint = "files:starlette_app"
interface = "asgi"
"""
    )


@pytest.fixture(scope="module")
def layout(tmp_path_factory: pytest.TempPathFactory) -> tuple[Path, Path, Path]:
    """The files, a directory outside the roots, and the project."""

    base = tmp_path_factory.mktemp("hints")
    files = base / "files"
    write_files(files)
    outside = base / "outside"
    outside.mkdir()
    (outside / "secret.txt").write_bytes(b"not for serving\n")
    project = base / "project"
    write_project(project, files)
    return files, outside, project


@pytest.fixture(scope="module")
def development(layout: tuple[Path, Path, Path]) -> Iterator[support.Server]:
    files, _, project = layout
    instance = support.start(
        str(project), workers=1, threads=None, chdir=None, env={"FILES_DIR": str(files)}
    )

    try:
        yield instance
    finally:
        instance.stop()


def request(
    port: int, path: str, headers: dict[str, str] | None = None, method: str = "GET"
) -> support.Response:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)

    try:
        connection.request(method, path, headers=headers or {})
        raw = connection.getresponse()
        return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
    finally:
        connection.close()


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# ----- by descriptor: WSGI


def test_a_file_wrapper_returned_directly_passes_the_descriptor(
    development: support.Server,
) -> None:
    """The application's file is closed before the client has read the
    body: the edge is sending from its own copy of the descriptor, so the
    application thread is free (section 6.5)."""

    development.mark_log()
    client = socket.create_connection(("127.0.0.1", development.port), timeout=30)

    try:
        client.sendall(b"GET /wrapper/big.bin HTTP/1.1\r\nHost: files\r\n\r\n")
        head = b""

        while b"\r\n\r\n" not in head:
            head += client.recv(1)

        assert head.startswith(b"HTTP/1.1 200")

        # The 3 MiB body is still in flight, and the application has
        # already returned and closed its file.

        assert development.wait_for_log("MARKER_CLOSED big.bin", timeout=10)
        body = head.split(b"\r\n\r\n", 1)[1]
        client.settimeout(60)

        while True:
            chunk = client.recv(1 << 16)

            if not chunk:
                break

            body += chunk

            if body.endswith(b"0\r\n\r\n") and len(body) > len(BIG):
                break
    finally:
        client.close()

    # Chunked, since the application declared no length: strip the framing.

    decoded = b""

    while body:
        size, _, body = body.partition(b"\r\n")
        count = int(size, 16)

        if count == 0:
            break

        decoded += body[:count]
        body = body[count + 2 :]

    assert len(decoded) == len(BIG)
    assert digest(decoded) == digest(BIG)


def test_the_range_starts_at_the_position_and_covers_the_content_length(
    development: support.Server,
) -> None:
    response = development.get("/wrapper-range/big.bin?offset=1000&length=5000")
    assert response.status == 200
    assert response.header("Content-Length") == "5000"
    assert response.body == BIG[1000:6000]
    assert development.get("/wrapper-range/small.txt?offset=2&length=19").body == SMALL[2:]


def test_objects_without_a_regular_file_behind_them_are_iterated(
    development: support.Server,
) -> None:
    assert development.get("/wrapper-bytesio").body == b"from memory"

    # Output written before the wrapper is returned means the head has
    # gone: the file is iterated after it.

    response = development.get("/wrapper-after-write/small.txt")
    assert response.body == b"prefix:" + SMALL


# ----- by path


def test_a_path_hint_is_served_as_a_static_file(
    development: support.Server, layout: tuple[Path, Path, Path]
) -> None:
    response = development.get("/sendfile/small.txt")
    assert response.status == 200
    assert response.body == SMALL
    assert response.header("Content-Length") == str(len(SMALL))
    assert response.header("Content-Type") == "text/x-custom", "the application's type wins"
    assert response.header("X-App") == "yes", "the application's headers go with the file"
    assert response.header("X-Muru-Sendfile") is None
    assert response.header("Accept-Ranges") == "bytes"
    etag = response.header("ETag")
    assert etag and response.header("Last-Modified")

    # Conditional and range requests are the edge's, as for a static route.

    assert development.get("/sendfile/small.txt", {"If-None-Match": etag}).status == 304
    partial = development.get("/sendfile/big.bin", {"Range": "bytes=10-19"})
    assert partial.status == 206
    assert partial.body == BIG[10:20]
    assert partial.header("Content-Range") == f"bytes 10-19/{len(BIG)}"
    head = request(development.port, "/sendfile/big.bin", method="HEAD")
    assert head.status == 200
    assert head.header("Content-Length") == str(len(BIG))
    assert head.body == b""

    # A nested path, the type guessed when the application set none, and
    # any other status going out as the application chose, with the file.

    created = development.get("/sendfile-status/nested/inner.txt")
    assert created.status == 201
    assert created.body == b"inner\n"
    assert created.header("Content-Type") == "text/plain; charset=utf-8"


def test_paths_outside_the_roots_are_refused_and_missing_files_are_404(
    development: support.Server, layout: tuple[Path, Path, Path]
) -> None:
    files, outside, _ = layout
    development.mark_log()
    refused = development.get(f"/sendfile-path?path={outside / 'secret.txt'}")
    assert refused.status == 500
    assert b"sendfile_refused" in refused.body
    assert development.wait_for_log("outside the mount's sendfile_roots")

    # A relative path, and a climb out of the roots by way of `..`, are
    # refused on the canonical path too.

    assert development.get("/sendfile-path?path=small.txt").status == 500
    climbing = files / ".." / "outside" / "secret.txt"
    assert development.get(f"/sendfile-path?path={climbing}").status == 500

    missing = development.get("/sendfile/nowhere.txt")
    assert missing.status == 404
    assert b"sendfile_missing" in missing.body
    assert development.get(f"/sendfile-path?path={files / 'nested'}").status == 404, "a directory"


def test_a_second_header_is_honoured_only_where_the_mount_names_it(
    development: support.Server,
) -> None:
    served = development.get("/compat/small.txt")
    assert served.status == 200
    assert served.body == SMALL
    assert served.header("X-Sendfile") is None

    # The default mount names no second header: X-Sendfile is an ordinary
    # header there, passed on with the application's own body.

    passed = development.get("/x-sendfile/small.txt")
    assert passed.body == b"the application's own body"
    assert passed.header("X-Sendfile") is not None


# ----- by descriptor: ASGI


def test_asgi_pathsend_and_zerocopysend(development: support.Server) -> None:
    extensions = json.loads(development.get("/asgi/extensions").body)
    assert "http.response.pathsend" in extensions
    assert "http.response.zerocopysend" in extensions

    whole = development.get("/asgi/pathsend/big.bin")
    assert whole.status == 200
    assert len(whole.body) == len(BIG)
    assert digest(whole.body) == digest(BIG)

    assert development.get("/asgi/zerocopysend/big.bin").body == BIG
    ranged = development.get("/asgi/zerocopysend/big.bin?offset=100&count=50")
    assert ranged.body == BIG[100:150]
    assert development.get("/asgi/zerocopysend/small.txt?count=1000000").body == SMALL

    # A file range between body chunks, with more_body on both sides.

    assert development.get("/asgi/mixed/small.txt").body == b"head:" + SMALL[:10] + b":tail"


def test_starlette_file_response_uses_pathsend(development: support.Server) -> None:
    pytest.importorskip("starlette")
    response = development.get("/starlette/big.bin")
    assert response.status == 200
    assert response.header("Content-Length") == str(len(BIG))
    assert digest(response.body) == digest(BIG)

    partial = development.get("/starlette/big.bin", {"Range": "bytes=0-9"})
    assert partial.status == 206
    assert partial.body == BIG[:10]


# ----- under the daemon


def test_hints_from_a_release_and_a_trusted_upstream_under_the_daemon(
    tmp_path: Path, layout: tuple[Path, Path, Path]
) -> None:
    files, outside, project = layout
    upstream = Upstream("hinting")
    daemon = start_daemon(tmp_path, config_extra="")

    try:
        daemon.muructl(
            "deploy",
            "files",
            str(project),
            "--python",
            sys.executable,
            "--processes",
            "1",
            "--env",
            f"FILES_DIR={files}",
        )
        served = daemon.get("/sendfile/small.txt")
        assert served.status == 200
        assert served.body == SMALL
        assert daemon.get(f"/sendfile-path?path={outside / 'secret.txt'}").status == 500
        assert daemon.get("/compat/small.txt").body == SMALL
        assert daemon.get("/asgi/pathsend/small.txt").body == SMALL
        assert daemon.get("/wrapper/small.txt").body == SMALL

        # The applied keys round-trip through the export.

        exported = daemon.muructl("config", "export").stdout
        assert f'sendfile_roots = ["{files}"]' in exported
        assert 'sendfile_header = "X-Sendfile"' in exported

        config = tmp_path / "upstreams.toml"
        config.write_text(
            f"""[app.files.release.r1]
source = '{project}'
python = '{sys.executable}'
processes = 1
env = {{ FILES_DIR = '{files}' }}

[upstream.trusted]
addresses = ['127.0.0.1:{upstream.port}']
trust_muru_headers = true
sendfile_roots = ['{files}']

[upstream.plain]
addresses = ['127.0.0.1:{upstream.port}']

[host.site]
names = ['site.test']
listeners = ['127.0.0.1:{daemon.port}']
default = true

[[route]]
host = 'site'
path = '/'
app = 'files'

[[route]]
host = 'site'
path = '/trusted/'
upstream = 'trusted'

[[route]]
host = 'site'
path = '/plain/'
upstream = 'plain'
"""
        )
        daemon.muructl("apply", "-f", str(config))
        small = {"X-File": str(files / "small.txt")}
        deadline = time.monotonic() + 10

        while time.monotonic() < deadline:
            if request(daemon.port, "/trusted/sendfile", small).status == 200:
                break

            time.sleep(0.2)

        hinted = request(daemon.port, "/trusted/sendfile", small)
        assert hinted.status == 200
        assert hinted.body == SMALL
        assert hinted.header("X-App") == "up"
        assert hinted.header("X-Muru-Sendfile") is None
        refused = request(daemon.port, "/trusted/sendfile", {"X-File": str(outside / "secret.txt")})
        assert refused.status == 500

        # An untrusted upstream's hint is removed with its other X-Muru
        # headers, and its own body goes out.

        plain = request(daemon.port, "/plain/sendfile", small)
        assert plain.status == 200
        assert plain.body == b"never sent"
        assert plain.header("X-Muru-Sendfile") is None
    finally:
        daemon.stop()
        upstream.stop()
