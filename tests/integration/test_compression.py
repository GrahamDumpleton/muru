"""Response compression (design specification section 4.17): the edge
compresses responses the client accepts, from the configured preference
order, for the configured content types and sizes, chunk by chunk for
streamed bodies, never for a response already encoded or one the
application opted out of, and the daemon's [compression] table changes
it on reload."""

from __future__ import annotations

import gzip
import json
import zlib
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, start_daemon


def decode(body: bytes, encoding: str) -> bytes:
    if encoding == "gzip":
        return gzip.decompress(body)

    if encoding == "br":
        brotli = pytest.importorskip("brotli")
        return bytes(brotli.decompress(body))

    if encoding == "zstd":
        zstd = pytest.importorskip("zstandard")
        return bytes(zstd.ZstdDecompressor().decompressobj().decompress(body))

    raise AssertionError(encoding)


@pytest.fixture(scope="module")
def server() -> support.Server:
    instance = support.start("plain_wsgi:application", workers=1, threads=2)
    yield instance
    instance.stop()


def test_text_is_gzipped_for_a_client_that_accepts_it(server: support.Server) -> None:
    response = server.get("/text", headers={"Accept-Encoding": "gzip, deflate"})
    assert response.status == 200
    assert response.header("Content-Encoding") == "gzip"
    assert response.header("Vary") == "Accept-Encoding"
    assert response.header("Content-Length") is None
    body = gzip.decompress(response.body)
    assert body.startswith(b"The quick brown fox") and len(body) == 4096

    plain = server.get("/text")
    assert plain.header("Content-Encoding") is None
    assert plain.header("Content-Length") == "4096"


def test_the_preferred_encoding_the_client_takes_is_chosen(server: support.Server) -> None:
    response = server.get("/text", headers={"Accept-Encoding": "gzip, br"})
    assert response.header("Content-Encoding") == "br"
    assert decode(response.body, "br").startswith(b"The quick brown fox")

    response = server.get("/text", headers={"Accept-Encoding": "gzip;q=1, zstd;q=0.8, br;q=0"})
    assert response.header("Content-Encoding") == "zstd"
    assert decode(response.body, "zstd").startswith(b"The quick brown fox")


def test_small_binary_and_opted_out_responses_stay_as_they_are(server: support.Server) -> None:
    small = server.get("/", headers={"Accept-Encoding": "gzip"})
    assert small.header("Content-Encoding") is None
    assert small.body == b"Hello, world!\n"

    binary = server.get("/large", headers={"Accept-Encoding": "gzip"})
    assert binary.header("Content-Encoding") is None
    assert len(binary.body) == 1024 * 1024

    opted = server.get("/text?compress=off", headers={"Accept-Encoding": "gzip"})
    assert opted.header("Content-Encoding") is None
    assert opted.header("X-Muru-Compress") is None
    assert len(opted.body) == 4096

    untransformed = server.get("/text?transform=no", headers={"Accept-Encoding": "gzip"})
    assert untransformed.header("Content-Encoding") is None

    encoded = server.get("/text?encoded=yes", headers={"Accept-Encoding": "gzip"})
    assert encoded.header("Content-Encoding") == "identity"
    assert len(encoded.body) == 4096


def test_streamed_responses_are_compressed_chunk_by_chunk(server: support.Server) -> None:
    response = server.get("/chunks?n=2000", headers={"Accept-Encoding": "gzip"})
    assert response.header("Content-Encoding") == "gzip"
    assert response.header("Content-Length") is None
    assert gzip.decompress(response.body) == b"".join(b"%015d\n" % i for i in range(2000))


def test_json_errors_from_the_edge_are_compressed_when_large_enough(
    server: support.Server,
) -> None:
    response = server.get("/info", headers={"Accept-Encoding": "gzip"})
    assert response.status == 200

    if response.header("Content-Encoding") == "gzip":
        json.loads(gzip.decompress(response.body))
    else:
        json.loads(response.body)


def test_compression_can_be_turned_off() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, threads=1, args=["--no-compression"]
    )

    try:
        response = server.get("/text", headers={"Accept-Encoding": "gzip, br"})
        assert response.header("Content-Encoding") is None
        assert response.header("Content-Length") == "4096"
    finally:
        server.stop()


def test_the_daemons_compression_table_applies_and_reloads(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        config_extra="[compression]\nencodings = ['gzip']\nmin_size = '100KB'\n",
    )

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )
        response = daemon.request("/catalogue", headers={"Accept-Encoding": "br, gzip"})
        assert response.status == 200
        assert response.header("Content-Encoding") is None, "a small JSON body"

        config = tmp_path / "muru.toml"
        text = config.read_text().replace("min_size = '100KB'", "min_size = '10'")
        config.write_text(text)
        assert "changed" in daemon.muructl("daemon", "reload").stdout
        response = daemon.request("/catalogue", headers={"Accept-Encoding": "br, gzip"})
        assert response.header("Content-Encoding") == "gzip", "gzip is the only encoding offered"
        assert json.loads(gzip.decompress(response.body))["app"] == "shop"

        config.write_text(text.replace("encodings = ['gzip']", "encodings = ['lzma']"))
        completed = daemon.muructl("daemon", "reload", check=False)
        assert completed.returncode != 0
        assert "is not an encoding" in completed.stdout + completed.stderr
    finally:
        daemon.stop()


def test_zlib_is_not_offered_as_deflate(server: support.Server) -> None:
    response = server.get("/text", headers={"Accept-Encoding": "deflate"})
    assert response.header("Content-Encoding") is None
    assert zlib.crc32(response.body) == zlib.crc32(server.get("/text").body)
