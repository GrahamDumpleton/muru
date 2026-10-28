"""The steps of the two sites tutorial, with the tutorial's own files.

docs/tutorials/two-sites-on-one-node.md includes the files under
docs/tutorials/code/node, and this test serves them through the page's
steps, so a change that breaks one fails here rather than for a reader.
The listeners and the notes server take free ports rather than the
page's, and the certificate is self-signed by openssl rather than issued
by mkcert.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon

NODE = support.ROOT / "docs" / "tutorials" / "code" / "node"
NAMES = ("shop.localhost", "blog.localhost", "notes.localhost")


def make_certificate(directory: Path) -> Path:
    """One self-signed certificate for the three names, as the page's mkcert makes."""

    openssl = shutil.which("openssl")

    if openssl is None:
        support.unavailable("openssl is not installed")

    alternatives = ",".join(f"DNS:{name}" for name in NAMES)
    certificate = directory / "certs" / "sites.pem"
    subprocess.run(
        [
            str(openssl),
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-days",
            "2",
            "-subj",
            f"/CN={NAMES[0]}",
            "-addext",
            f"subjectAltName={alternatives}",
            "-keyout",
            str(directory / "certs" / "sites-key.pem"),
            "-out",
            str(certificate),
        ],
        check=True,
        capture_output=True,
    )

    return certificate


def visit(daemon: Daemon, name: str, cafile: Path | None = None, path: str = "/") -> str:
    port = daemon.https_port if cafile else daemon.port
    response = daemon.request(path, host=name, port=port, cafile=cafile)
    assert response.status == 200, (name, response.status, response.body)
    return response.body.decode()


def test_the_two_sites_tutorial(tmp_path: Path) -> None:
    node = tmp_path / "node"
    shutil.copytree(NODE, node)
    (node / "certs").mkdir()
    certificate = make_certificate(node)

    # The page's listeners: one plain, one TLS.

    listeners = tomllib.loads((node / "node.toml").read_text())["listener"]
    assert [listener.get("tls", False) for listener in listeners] == [False, True]

    notes_port = support.free_port()
    notes = subprocess.Popen(
        [
            sys.executable,
            "-m",
            "http.server",
            str(notes_port),
            "--bind",
            "127.0.0.1",
            "--directory",
            str(node / "notes"),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(
        tmp_path / "daemon",
        listeners=[(support.free_port(), False), (support.free_port(), True)],
    )

    try:
        out = daemon.deploy("shop", node / "shop", "--entrypoint", "app:application")
        assert out == "muru: release shop@r1 is ready and serving"
        sites = (
            (node / "sites.toml")
            .read_text()
            .replace("/home/you/node", str(node))
            .replace("127.0.0.1:9001", f"127.0.0.1:{notes_port}")
        )
        (node / "sites.toml").write_text(sites)
        applied = daemon.muructl("apply", "-f", str(node / "sites.toml")).stdout
        assert "create-upstream upstream notes           created" in applied
        support.wait_for_port(notes_port, notes)

        # Visit the sites, over HTTPS by name.

        assert (
            visit(daemon, "shop.localhost", certificate) == "The shop, at https://shop.localhost\n"
        )
        assert "<title>The blog</title>" in visit(daemon, "blog.localhost", certificate)
        assert "<title>Notes</title>" in visit(daemon, "notes.localhost", certificate)
        assert "<title>The blog</title>" in visit(daemon, "blog.localhost")

        # The shop sends plain HTTP to HTTPS; an unknown name reaches nothing.

        redirect = daemon.request("/cart", host="shop.localhost")
        assert redirect.status == 307
        assert redirect.header("location") == f"https://shop.localhost:{daemon.https_port}/cart"
        assert daemon.request("/", host="other.example").status == 421

        # When the notes server is down, the notes site is 502 and the
        # others carry on.

        notes.terminate()
        notes.wait()
        assert daemon.request("/", host="notes.localhost").status == 502
        assert visit(daemon, "shop.localhost", certificate).startswith("The shop")

        # What the daemon holds.

        hosts = daemon.muructl("host", "list").stdout
        assert "shop             yes      all        shop.localhost" in hosts
        assert "upstream notes" in daemon.muructl("route", "list").stdout
        exported = daemon.muructl("config", "export").stdout
        assert "[upstream.notes]" in exported and "[host.blog]" in exported
    finally:
        if notes.poll() is None:
            notes.terminate()
            notes.wait()

        daemon.stop()
