"""Static files (design specification section 4.9): `muructl serve-static`
and static routes under the daemon serve a directory with index files,
ETag and Last-Modified, conditional requests, ranges, precompressed
variants, cache headers and a fallback, never a hidden file, a path that
climbs out, or a name that differs from the file's on disk."""

from __future__ import annotations

import gzip
import os
import subprocess
import sys
import time
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, start_daemon

STATIC = support.ROOT / "tests" / "fixtures" / "static"


@dataclass
class Static:
    """A `muructl serve-static` process."""

    port: int
    process: subprocess.Popen[bytes]

    def get(self, path: str, headers: dict[str, str] | None = None) -> support.Response:
        return request("GET", self.port, path, headers)

    def stop(self) -> None:
        self.process.terminate()
        self.process.wait(timeout=30)


def request(
    method: str, port: int, path: str, headers: dict[str, str] | None = None
) -> support.Response:
    import http.client

    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)

    try:
        connection.request(method, path, headers=headers or {})
        raw = connection.getresponse()
        return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
    finally:
        connection.close()


def serve_static(directory: Path, *arguments: str) -> Static:
    muructl = support.muructl_path()

    if muructl is None:
        support.unavailable("muructl is not built (cargo build -p muructl)")

    port = support.free_port()
    process = subprocess.Popen(
        [str(muructl), "serve-static", str(directory), "--port", str(port), *arguments],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    support.wait_for_port(port, process)
    return Static(port, process)


@pytest.fixture(scope="module")
def static() -> Iterator[Static]:
    instance = serve_static(STATIC)
    yield instance
    instance.stop()


def test_files_and_index_files_are_served_with_their_types(static: Static) -> None:
    index = static.get("/")
    assert index.status == 200
    assert index.header("Content-Type") == "text/html; charset=utf-8"
    assert b"Hello from the static fixture" in index.body
    assert index.header("Content-Length") == str(len(index.body))
    assert index.header("Accept-Ranges") == "bytes"
    assert index.header(
        "ETag",
    ) and index.header("Last-Modified")

    assert static.get("/index.html").body == index.body
    assert static.get("/styles/site.css").header("Content-Type") == "text/css; charset=utf-8"
    assert static.get("/nested/deep/file.txt").body == b"deep file\n"
    assert static.get("/Logo.PNG").header("Content-Type") == "image/png"
    assert (
        static.get("/docs/").status == 200 and b"<title>Docs</title>" in static.get("/docs/").body
    )

    head = request("HEAD", static.port, "/fox.txt")
    assert head.status == 200 and head.body == b"" and head.header("Content-Length") == "45"


def test_directories_redirect_to_their_slash_form(static: Static) -> None:
    response = static.get("/docs")
    assert response.status == 301
    assert response.header("Location") == "/docs/"
    assert static.get("/docs?x=1").header("Location") == "/docs/?x=1"


def test_conditional_requests_are_answered_304(static: Static) -> None:
    first = static.get("/fox.txt")
    etag = first.header("ETag")
    modified = first.header("Last-Modified")
    assert etag and modified

    assert static.get("/fox.txt", {"If-None-Match": etag}).status == 304
    assert static.get("/fox.txt", {"If-None-Match": '"other", ' + etag}).status == 304
    assert static.get("/fox.txt", {"If-Modified-Since": modified}).status == 304
    assert static.get("/fox.txt", {"If-None-Match": '"other"'}).status == 200
    assert (
        static.get("/fox.txt", {"If-Modified-Since": "Sat, 01 Jan 2000 00:00:00 GMT"}).status == 200
    )


def test_ranges_are_served_and_bad_ones_refused(static: Static) -> None:
    part = static.get("/fox.txt", {"Range": "bytes=4-8"})
    assert part.status == 206
    assert part.body == b"quick"
    assert part.header("Content-Range") == "bytes 4-8/45"
    assert part.header("Content-Length") == "5"

    tail = static.get("/fox.txt", {"Range": "bytes=-5"})
    assert tail.status == 206 and tail.body == b"dog.\n"
    assert static.get("/fox.txt", {"Range": "bytes=40-"}).body == b"dog.\n"

    bad = static.get("/fox.txt", {"Range": "bytes=100-"})
    assert bad.status == 416
    assert bad.header("Content-Range") == "bytes */45"

    whole = static.get("/fox.txt", {"Range": "bytes=0-1,3-4"})
    assert whole.status == 200 and len(whole.body) == 45


def test_precompressed_variants_are_preferred_when_accepted(static: Static) -> None:
    plain = static.get("/app.js")
    assert plain.status == 200 and plain.header("Content-Encoding") is None
    assert plain.body == (STATIC / "app.js").read_bytes()

    gzipped = static.get("/app.js", {"Accept-Encoding": "gzip"})
    assert gzipped.header("Content-Encoding") == "gzip"
    assert gzipped.header("Vary") == "Accept-Encoding"
    assert gzipped.header("Content-Type") == "text/javascript; charset=utf-8"
    assert gzipped.body == (STATIC / "app.js.gz").read_bytes()
    assert gzip.decompress(gzipped.body) == plain.body

    brotlied = static.get("/app.js", {"Accept-Encoding": "gzip, br"})
    assert brotlied.header("Content-Encoding") == "br"
    assert brotlied.body == (STATIC / "app.js.br").read_bytes()

    # No variant next to the file: compressed on the fly when large
    # enough, else as it is.

    ranged = static.get("/app.js", {"Accept-Encoding": "gzip", "Range": "bytes=0-3"})
    assert ranged.status == 206 and ranged.header("Content-Encoding") is None


def test_hidden_climbing_and_differently_cased_paths_are_not_served(static: Static) -> None:
    assert static.get("/.secret").status == 404
    assert static.get("/nested/../fox.txt").status == 404
    assert static.get("/nested/%2e%2e/fox.txt").status == 404
    assert static.get("/logo.png").status == 404, "the file is Logo.PNG"
    assert static.get("/LOGO.png").status == 404
    assert static.get("/missing.txt").status == 404
    assert static.get("/fox.txt/").status == 404

    response = request("POST", static.port, "/fox.txt")
    assert response.status == 405
    assert response.header("Allow") == "GET, HEAD"


def test_a_symbolic_link_leaving_the_directory_is_refused(tmp_path: Path) -> None:
    site = tmp_path / "site"
    site.mkdir()
    (site / "inside.txt").write_text("inside\n")
    outside = tmp_path / "outside.txt"
    outside.write_text("outside\n")
    os.symlink(outside, site / "escape.txt")
    os.symlink(site / "inside.txt", site / "alias.txt")
    instance = serve_static(site)

    try:
        assert instance.get("/inside.txt").body == b"inside\n"
        assert instance.get("/alias.txt").body == b"inside\n"
        assert instance.get("/escape.txt").status == 404
    finally:
        instance.stop()


def test_a_fallback_file_serves_a_single_page_application() -> None:
    instance = serve_static(STATIC, "--fallback", "index.html")

    try:
        assert instance.get("/fox.txt").body.startswith(b"The quick")
        shell = instance.get("/some/client/route")
        assert shell.status == 200
        assert b"Hello from the static fixture" in shell.body
        assert instance.get("/.secret").status == 200, "hidden paths fall back too"
    finally:
        instance.stop()


def test_static_routes_under_the_daemon_with_cache_headers_and_an_app_fallback(
    tmp_path: Path,
) -> None:
    daemon = start_daemon(tmp_path)

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )
        config = tmp_path / "site.toml"
        config.write_text(
            "[host.site]\n"
            "names = ['site.test']\n"
            "default = true\n\n"
            "[[route]]\n"
            "host = 'site'\n"
            "path = '/'\n"
            f"static = '{STATIC}'\n"
            "fallback = { app = 'shop' }\n"
            "cache = { max_age = '1y', immutable = true, except = ['index.html'] }\n\n"
            "[[route]]\n"
            "host = 'site'\n"
            "path = '/app/'\n"
            "app = 'shop'\n"
        )
        applied = daemon.muructl("apply", "-f", str(config), check=False)
        assert applied.returncode == 0, applied.stdout + applied.stderr

        listed = daemon.muructl("route", "list").stdout
        assert f"static {STATIC}, else shop" in listed

        # Files come with the cache headers; what has no file reaches the
        # application, as does every method files do not answer.

        css = daemon.request("/styles/site.css", host="site.test")
        assert css.status == 200
        assert css.header("Cache-Control") == "public, max-age=31536000, immutable"
        assert daemon.request("/", host="site.test").header("Cache-Control") == "no-cache"
        catalogue = daemon.request("/catalogue", host="site.test")
        assert catalogue.status == 200 and b'"app": "shop"' in catalogue.body
        assert daemon.request("/app/anything", host="site.test").status == 200
        posted = request("POST", daemon.port, "/fox.txt", {"Host": "site.test"})
        assert posted.status == 200, "a POST goes to the application even where a file exists"
        assert daemon.request("/.secret", host="site.test").status == 200, "hidden paths fall back"

        # The export round-trips the static route.

        exported = daemon.muructl("config", "export").stdout
        assert f"static = '{STATIC}'" in exported or f'static = "{STATIC}"' in exported
        assert "immutable = true" in exported

        # A route cannot name both an application and a directory, and a
        # missing directory is refused.

        config.write_text(
            "[[route]]\nhost = 'site'\npath = '/both/'\napp = 'shop'\nstatic = '/tmp'\n"
            "[[route]]\nhost = 'site'\npath = '/gone/'\nstatic = '/no/such/directory'\n"
        )
        refused = daemon.muructl("apply", "-f", str(config), check=False)
        assert refused.returncode == 1
        text = refused.stdout + refused.stderr
        assert "names both an application and a static directory" in text
        assert "is not a directory" in text
    finally:
        daemon.stop()


def test_serve_static_refuses_what_is_not_a_directory(tmp_path: Path) -> None:
    muructl = support.muructl_path()
    assert muructl is not None
    completed = subprocess.run(
        [str(muructl), "serve-static", str(tmp_path / "missing")],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert completed.returncode != 0
    assert "is not a directory" in completed.stderr
    time.sleep(0)
    assert sys.executable
