"""HTTPS in development (design specification section 13.1).

`muructl run --https` serves TLS next to plain HTTP with a certificate it
makes itself: issued by the local mkcert authority when one is installed,
self-signed otherwise. The tests point MURU_CACHE_DIR at a fresh directory
and CAROOT at an empty one, which forces the self-signed path, so a
developer's own mkcert authority is neither used nor touched.
"""

from __future__ import annotations

import json
import shutil
import ssl
import subprocess
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support

CURL = shutil.which("curl")


@pytest.fixture(scope="module")
def cache(tmp_path_factory: pytest.TempPathFactory) -> Path:
    return tmp_path_factory.mktemp("cache")


@pytest.fixture(scope="module")
def no_authority(tmp_path_factory: pytest.TempPathFactory) -> dict[str, str]:
    """Environment that makes muructl look for mkcert where it is not."""

    return {"CAROOT": str(tmp_path_factory.mktemp("caroot"))}


@pytest.fixture(scope="module")
def server(cache: Path, no_authority: dict[str, str]) -> Iterator[support.Server]:
    instance = support.start(
        "plain_wsgi:application",
        args=["--https", "--host", "shop.localhost"],
        env={"MURU_CACHE_DIR": str(cache), **no_authority},
        https=True,
    )

    try:
        yield instance
    finally:
        instance.stop()


def certificate(cache: Path) -> Path:
    return cache / "certs" / "dev.pem"


def environ_over_tls(server: support.Server, cafile: Path, hostname: str) -> dict[str, object]:
    response = server.request_tls("GET", "/info", cafile, server_hostname=hostname)
    assert response.status == 200, response.body
    environ: dict[str, object] = json.loads(response.body)["environ"]
    return environ


def test_https_request_reaches_the_application_with_tls_details(
    server: support.Server, cache: Path
) -> None:
    environ = environ_over_tls(server, certificate(cache), "localhost")
    assert environ["wsgi.url_scheme"] == "https"
    assert environ["HTTPS"] == "on"
    assert environ["SERVER_PORT"] == str(server.https_port)
    tls = environ["muru.tls"]
    assert isinstance(tls, dict)
    assert tls["version"] in {"TLSv1.3", "TLSv1.2"}
    assert tls["cipher"].startswith("TLS")
    assert tls["server_name"] == "localhost"

    # http.client offers no application protocol, so none is negotiated.

    assert tls["alpn"] == ""


def test_plain_http_is_still_served(server: support.Server) -> None:
    response = server.get("/info")
    assert response.status == 200
    environ = json.loads(response.body)["environ"]
    assert environ["wsgi.url_scheme"] == "http"
    assert "HTTPS" not in environ
    assert "muru.tls" not in environ


def test_certificate_covers_the_default_and_extra_names(
    server: support.Server, cache: Path
) -> None:
    for hostname in ("localhost", "127.0.0.1", "shop.localhost"):
        environ = environ_over_tls(server, certificate(cache), hostname)
        expected_sni = "" if hostname == "127.0.0.1" else hostname
        assert environ["muru.tls"]["server_name"] == expected_sni, hostname  # type: ignore[index]


def test_an_unlisted_name_fails_verification(server: support.Server, cache: Path) -> None:
    with pytest.raises(ssl.SSLCertVerificationError):
        server.request_tls("GET", "/", certificate(cache), server_hostname="other.localhost")


def test_self_signed_certificate_is_announced_with_advice(server: support.Server) -> None:
    log = server.log()
    assert (
        "muru: self-signed certificate for localhost, 127.0.0.1, ::1, shop.localhost (created"
        in log
    )
    assert "install mkcert" in log


@pytest.mark.skipif(CURL is None, reason="curl is needed")
def test_http2_is_negotiated_by_alpn(server: support.Server, cache: Path) -> None:
    assert CURL is not None
    version = subprocess.run([CURL, "--version"], capture_output=True, text=True, check=False)
    if "HTTP2" not in version.stdout:
        pytest.skip("curl without HTTP/2 support")
    completed = subprocess.run(
        [
            CURL,
            "-sS",
            "--cacert",
            str(certificate(cache)),
            "--http2",
            "-w",
            "\n%{http_version}",
            f"https://localhost:{server.https_port}/info",
        ],
        capture_output=True,
        check=False,
        timeout=30,
    )
    assert completed.returncode == 0, completed.stderr.decode()
    body, _, http_version = completed.stdout.rpartition(b"\n")
    assert http_version == b"2"
    environ = json.loads(body)["environ"]
    assert environ["SERVER_PROTOCOL"] == "HTTP/2.0"
    assert environ["muru.tls"]["alpn"] == "h2"


def test_certificate_is_reused_and_can_be_given_explicitly(
    server: support.Server, cache: Path, no_authority: dict[str, str]
) -> None:
    # The module's server made the certificate; a second run must reuse it.

    before = certificate(cache).read_bytes()
    again = support.start(
        "plain_wsgi:application",
        args=["--https"],
        env={"MURU_CACHE_DIR": str(cache), **no_authority},
        https=True,
    )

    try:
        assert again.request_tls("GET", "/", certificate(cache)).status == 200
        assert "(reused, " in again.log()
    finally:
        again.stop()

    assert certificate(cache).read_bytes() == before

    explicit = support.start(
        "plain_wsgi:application",
        args=[
            "--tls-cert",
            str(certificate(cache)),
            "--tls-key",
            str(cache / "certs" / "dev-key.pem"),
        ],
        env={"MURU_CACHE_DIR": str(cache), **no_authority},
        https=True,
    )

    try:
        assert explicit.request_tls("GET", "/", certificate(cache)).status == 200
        assert f"muru: TLS certificate from {certificate(cache)}" in explicit.log()
    finally:
        explicit.stop()


def test_https_only_redirects_plain_http(cache: Path, no_authority: dict[str, str]) -> None:
    instance = support.start(
        "plain_wsgi:application",
        args=["--https-only"],
        env={"MURU_CACHE_DIR": str(cache), **no_authority},
        https=True,
    )

    try:
        response = instance.get("/a/b?x=1&y=2", headers={"Host": f"localhost:{instance.port}"})
        assert response.status == 307
        assert response.header("Location") == f"https://localhost:{instance.https_port}/a/b?x=1&y=2"
        assert response.header("Content-Length") == "0"
        posted = instance.post("/submit", b"payload", headers={"Host": "127.0.0.1"})
        assert posted.status == 307
        assert posted.header("Location") == f"https://127.0.0.1:{instance.https_port}/submit"
        assert instance.request_tls("GET", "/", certificate(cache)).status == 200
        assert "redirects to it" in instance.log()
    finally:
        instance.stop()


def mkcert_root() -> Path | None:
    mkcert = shutil.which("mkcert")
    if mkcert is None:
        return None
    completed = subprocess.run([mkcert, "-CAROOT"], capture_output=True, text=True, check=False)
    root = Path(completed.stdout.strip())
    if completed.returncode != 0 or not (root / "rootCA.pem").is_file():
        return None
    return root


@pytest.mark.skipif(mkcert_root() is None, reason="an mkcert authority is needed")
def test_mkcert_authority_issues_the_certificate(tmp_path: Path) -> None:
    root = mkcert_root()
    assert root is not None
    instance = support.start(
        "plain_wsgi:application",
        args=["--https"],
        env={"MURU_CACHE_DIR": str(tmp_path), "CAROOT": str(root)},
        https=True,
    )

    try:
        response = instance.request_tls("GET", "/", root / "rootCA.pem")
        assert response.status == 200
        assert f"issued by the mkcert authority in {root}" in instance.log()
    finally:
        instance.stop()
