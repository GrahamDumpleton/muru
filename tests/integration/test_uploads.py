"""Upload offloading (design specification section 4.11).

Routes with `upload` set make the edge receive the body first and store
each file: a multipart form reaches the application with its file fields
replaced by text fields that describe them, a raw upload with an empty
body, and the files themselves through `muru.uploads`. The limits,
quotas and the precheck answer at the edge. One daemon serves every test
here, with routes applied for each behaviour.
"""

from __future__ import annotations

import email.parser
import email.policy
import hashlib
import http.client
import json
import shutil
import socket
import sys
import time
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon

FIXTURE = support.ROOT / "tests" / "fixtures" / "projects" / "uploads"

BOUNDARY = "muru-test-boundary"

FORM_TYPE = f"multipart/form-data; boundary={BOUNDARY}"

DOCUMENT = bytes(range(256)) * 12


@dataclass
class Site:
    daemon: Daemon
    base: Path
    prechecks: Path

    @property
    def spool(self) -> Path:
        return self.base / "daemon" / "state" / "spool" / "uploads"


@pytest.fixture(scope="module")
def site(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Site]:
    base = tmp_path_factory.mktemp("uploads")
    project = base / "project"
    shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
    (project / "muru.toml").write_text(
        'entrypoint = "uploads:wsgi_app"\nthreads = 2\n'
        + "".join(
            f'\n[mounts.{name}]\npath = "/{name}"\nentrypoint = "{entrypoint}"\n'
            f'interface = "{interface}"\n'
            for name, entrypoint, interface in [
                ("asgi", "uploads:asgi_app", "asgi"),
                ("flask", "flask_app:app", "wsgi"),
                ("starlette", "starlette_app:app", "asgi"),
                ("django", "django_app:application", "wsgi"),
            ]
        )
    )
    prechecks = base / "prechecks"
    prechecks.touch()
    (base / "daemon").mkdir()
    daemon = start_daemon(base / "daemon")

    try:
        config = base / "site.toml"
        config.write_text(
            f"""[app.uploads.release.r1]
source = '{project}'
python = '{sys.executable}'
processes = 1
env = {{ PRECHECKS = '{prechecks}' }}

[host.site]
names = ['site.test']
listeners = ['127.0.0.1:{daemon.port}']
default = true

[[route]]
host = 'site'
path = '/'
app = 'uploads'

[[route]]
host = 'site'
path = '/multipart/'
app = 'uploads'
upload = 'multipart'
max_file_size = '64KB'
max_files = 3
hash = ['sha256', 'sha1']

[[route]]
host = 'site'
path = '/raw/'
app = 'uploads'
upload = 'raw'

[[route]]
host = 'site'
path = '/big/'
app = 'uploads'
upload = 'raw'
max_body_size = '20MB'
hash = []

[[route]]
host = 'site'
path = '/quota/'
app = 'uploads'
upload = 'raw'
spool_quota = '64KB'

[[route]]
host = 'site'
path = '/precheck/'
app = 'uploads'
upload = 'multipart'
precheck = true

[[route]]
host = 'site'
path = '/asgi/'
app = 'uploads'
upload = 'multipart'
"""
            + "".join(
                f"\n[[route]]\nhost = 'site'\npath = '/{name}/'\napp = 'uploads'\n"
                "upload = 'multipart'\n"
                for name in ("flask", "starlette", "django")
            )
        )
        daemon.muructl("apply", "-f", str(config))
        daemon.muructl("release", "wait", "uploads@r1", "--timeout", "60")
        yield Site(daemon, base, prechecks)
    finally:
        daemon.stop()


def form(fields: list[tuple[str, str | tuple[str, str, bytes]]]) -> bytes:
    """A multipart/form-data body: text fields, and (filename, type, content) files."""

    parts = []

    for name, value in fields:
        if isinstance(value, tuple):
            filename, content_type, content = value
            parts.append(
                f'--{BOUNDARY}\r\nContent-Disposition: form-data; name="{name}"; '
                f'filename="{filename}"\r\nContent-Type: {content_type}\r\n\r\n'.encode()
                + content
                + b"\r\n"
            )
        else:
            parts.append(
                f'--{BOUNDARY}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n'
                f"{value}\r\n".encode()
            )

    return b"".join(parts) + f"--{BOUNDARY}--\r\n".encode()


def parse_form(content_type: str, body: bytes) -> list[tuple[str, str]]:
    """The fields of a multipart body, in order, as name and text."""

    message = email.parser.BytesParser(policy=email.policy.default).parsebytes(
        f"Content-Type: {content_type}\r\n\r\n".encode() + body
    )
    fields = []

    for part in message.iter_parts():
        name = part.get_param("name", header="content-disposition")
        payload = part.get_payload(decode=True)
        fields.append((str(name), payload.decode("latin-1")))

    return fields


def post(
    site: Site,
    path: str,
    body: bytes | Iterable[bytes],
    headers: dict[str, str],
    chunked: bool = False,
) -> support.Response:
    connection = http.client.HTTPConnection("127.0.0.1", site.daemon.port, timeout=60)

    try:
        connection.request("POST", path, body=body, headers=headers, encode_chunked=chunked)
        raw = connection.getresponse()
        return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
    finally:
        connection.close()


def exchange(site: Site, head: str, body: bytes | None) -> tuple[list[bytes], bytes]:
    """Send a request head with Expect: 100-continue, then the body only
    once 100 Continue arrives. Returns the status lines seen and the rest.
    """

    with socket.create_connection(("127.0.0.1", site.daemon.port), timeout=30) as connection:
        connection.sendall(head.encode())
        received = b""
        statuses: list[bytes] = []

        while True:
            chunk = connection.recv(65536)

            if not chunk:
                break

            received += chunk

            # Each response head is complete at its blank line; a 100
            # Continue is followed by the body, a final status ends it.

            while b"\r\n\r\n" in received:
                header, _, received = received.partition(b"\r\n\r\n")
                statuses.append(header.split(b"\r\n", 1)[0])

                if b" 100 " in statuses[-1] and body is not None:
                    connection.sendall(body)
                elif b" 100 " not in statuses[-1]:
                    return statuses, received + read_rest(connection)

        return statuses, received


def read_rest(connection: socket.socket) -> bytes:
    rest = b""

    while chunk := connection.recv(65536):
        rest += chunk

    return rest


def test_multipart_file_fields_are_replaced_by_their_description(site: Site) -> None:
    avatar = b"avatar bytes"
    body = form(
        [
            ("title", "hello"),
            ("avatar", ("a.txt", "text/plain", avatar)),
            ("doc", ("b.bin", "application/octet-stream", DOCUMENT)),
            ("note", "after"),
        ]
    )
    response = post(site, "/multipart/describe", body, {"Content-Type": FORM_TYPE})
    assert response.status == 200, response.body
    seen = json.loads(response.body)

    # The application's form: the ordinary fields as they were, each file
    # as text fields where it stood, and no file content.

    assert seen["content_length"] == str(len(seen["body"].encode("latin-1")))
    fields = parse_form(seen["content_type"], seen["body"].encode("latin-1"))
    assert fields == [
        ("title", "hello"),
        ("avatar.name", "a.txt"),
        ("avatar.content_type", "text/plain"),
        ("avatar.size", str(len(avatar))),
        ("avatar.sha256", hashlib.sha256(avatar).hexdigest()),
        ("avatar.sha1", hashlib.sha1(avatar).hexdigest()),
        ("avatar.ref", "1"),
        ("doc.name", "b.bin"),
        ("doc.content_type", "application/octet-stream"),
        ("doc.size", str(len(DOCUMENT))),
        ("doc.sha256", hashlib.sha256(DOCUMENT).hexdigest()),
        ("doc.sha1", hashlib.sha1(DOCUMENT).hexdigest()),
        ("doc.ref", "2"),
        ("note", "after"),
    ]

    # The files, in form order, readable and seekable.

    first, second = seen["uploads"]
    assert first["field"] == "avatar"
    assert first["name"] == "a.txt"
    assert first["content"] == "avatar bytes"
    assert first["moved"] == "ata"
    assert first["sha256"] == hashlib.sha256(avatar).hexdigest()
    assert set(first["digests"]) == {"sha256", "sha1"}
    assert second["field"] == "doc"
    assert second["ref"] == "2"
    assert second["content"].encode("latin-1") == DOCUMENT


def test_an_upload_is_found_by_its_ref(site: Site) -> None:
    body = form([("one", ("1.txt", "text/plain", b"first")), ("two", ("2.txt", "", b"second"))])
    response = post(site, "/multipart/get?ref=2", body, {"Content-Type": FORM_TYPE})
    found = json.loads(response.body)["found"]
    assert found["field"] == "two"
    assert found["content"] == "second"
    assert found["content_type"] == ""

    missing = post(site, "/multipart/get?ref=9", body, {"Content-Type": FORM_TYPE})
    assert json.loads(missing.body)["found"] is None


def test_a_raw_upload_is_one_file_and_an_empty_body(site: Site) -> None:
    content = DOCUMENT * 50
    response = post(
        site,
        "/raw/describe",
        content,
        {
            "Content-Type": "application/octet-stream",
            "Content-Disposition": 'attachment; filename="data.bin"',
        },
    )
    assert response.status == 200, response.body
    seen = json.loads(response.body)
    assert seen["body"] == ""
    assert seen["content_length"] == "0"
    (upload,) = seen["uploads"]
    assert upload["field"] is None
    assert upload["name"] == "data.bin"
    assert upload["content_type"] == "application/octet-stream"
    assert upload["size"] == len(content)
    assert upload["sha256"] == hashlib.sha256(content).hexdigest()
    assert upload["content"].encode("latin-1") == content


def test_a_chunked_upload_is_received_whole(site: Site) -> None:
    pieces = [DOCUMENT[:100], DOCUMENT[100:1000], DOCUMENT[1000:]]
    response = post(
        site, "/raw/describe", iter(pieces), {"Content-Type": "text/plain"}, chunked=True
    )
    assert response.status == 200, response.body
    (upload,) = json.loads(response.body)["uploads"]
    assert upload["content"].encode("latin-1") == DOCUMENT
    assert upload["name"] == ""


def test_a_body_that_is_not_a_form_passes_through(site: Site) -> None:
    response = post(
        site, "/multipart/describe", b'{"plain": true}', {"Content-Type": "application/json"}
    )
    seen = json.loads(response.body)
    assert seen["uploads"] == []
    assert seen["body"] == '{"plain": true}'


def test_saved_files_are_linked_and_unsaved_ones_vanish(site: Site, tmp_path: Path) -> None:
    body = form([("a", ("a.txt", "text/plain", b"kept")), ("b", ("b.txt", "text/plain", b"too"))])
    response = post(site, f"/multipart/save?dest={tmp_path}", body, {"Content-Type": FORM_TYPE})
    assert response.status == 200, response.body
    saved = json.loads(response.body)["saved"]
    assert [entry["content"] for entry in saved] == ["kept", "too"]
    assert [entry["again"] for entry in saved] == ["exists", "exists"]
    assert [entry["replaced"] for entry in saved] == [
        str(tmp_path / "1.bin"),
        str(tmp_path / "2.bin"),
    ]

    # The same user and the same file system: linked, not copied.

    assert all(entry["linked"] for entry in saved)
    assert (tmp_path / "1.bin").read_bytes() == b"kept"
    assert (tmp_path / "2.bin").read_bytes() == b"too"

    # Nothing is left in the spool once the request has ended.

    deadline = time.monotonic() + 5

    while time.monotonic() < deadline and any(site.spool.iterdir()):
        time.sleep(0.1)

    assert list(site.spool.iterdir()) == []


def test_upload_path_opens_the_file(site: Site) -> None:
    body = form([("a", ("a.txt", "text/plain", b"by path"))])
    response = post(site, "/multipart/path", body, {"Content-Type": FORM_TYPE})
    assert json.loads(response.body)["contents"] == ["by path"]


@pytest.mark.parametrize(
    ("fields", "status", "reason"),
    [
        ([("big", ("big.bin", "", b"x" * (65 * 1024)))], 413, b"upload_too_large"),
        ([(f"f{index}", (f"{index}.txt", "", b"x")) for index in range(4)], 413, b"too_many_files"),
    ],
    ids=["max_file_size", "max_files"],
)
def test_limits_of_the_route_are_answered_at_the_edge(
    site: Site,
    fields: list[tuple[str, str | tuple[str, str, bytes]]],
    status: int,
    reason: bytes,
) -> None:
    response = post(site, "/multipart/describe", form(fields), {"Content-Type": FORM_TYPE})
    assert response.status == status
    assert reason in response.body


def test_a_malformed_form_is_400(site: Site) -> None:
    body = f'--{BOUNDARY}\r\nContent-Disposition: form-data; name="a"\r\n\r\nno end'.encode()
    response = post(site, "/multipart/describe", body, {"Content-Type": FORM_TYPE})
    assert response.status == 400
    assert b"upload_malformed" in response.body


def test_the_spool_quota_is_507(site: Site) -> None:
    response = post(site, "/quota/describe", b"x" * (100 * 1024), {"Content-Type": "text/plain"})
    assert response.status == 507
    assert b"upload_spool_full" in response.body

    # What the refused upload held is given back.

    within = post(site, "/quota/describe", b"x" * 1024, {"Content-Type": "text/plain"})
    assert within.status == 200


def test_a_route_may_raise_the_body_limit(site: Site) -> None:
    content = b"y" * (11 * 1024 * 1024)

    # Over the edge's own 10MB, refused from the declared length alone.

    statuses, _ = exchange(
        site,
        f"POST /raw/describe HTTP/1.1\r\nHost: site.test\r\nContent-Length: {len(content)}\r\n"
        "Expect: 100-continue\r\nConnection: close\r\n\r\n",
        None,
    )
    assert statuses[-1].startswith(b"HTTP/1.1 413")

    response = post(site, "/big/describe", content, {"Content-Type": "text/plain"})
    assert response.status == 200, response.body[:200]
    (upload,) = json.loads(response.body)["uploads"]
    assert upload["size"] == len(content)
    assert upload["digests"] == {}


def test_a_precheck_comes_before_the_body(site: Site) -> None:
    body = form([("a", ("a.txt", "text/plain", b"checked"))])
    head = (
        f"POST /precheck/describe HTTP/1.1\r\nHost: site.test\r\nContent-Type: {FORM_TYPE}\r\n"
        f"Content-Length: {len(body)}\r\nExpect: 100-continue\r\nX-Tag: allowed\r\n"
        "Connection: close\r\n\r\n"
    )
    statuses, rest = exchange(site, head, body)
    assert statuses[0].startswith(b"HTTP/1.1 100")
    assert statuses[-1].startswith(b"HTTP/1.1 200")
    assert b'"content": "checked"' in rest
    assert "allowed" in site.prechecks.read_text().split()


def test_a_refused_precheck_is_the_answer_and_the_body_is_never_asked_for(site: Site) -> None:
    head = (
        f"POST /precheck/describe HTTP/1.1\r\nHost: site.test\r\nContent-Type: {FORM_TYPE}\r\n"
        "Content-Length: 1000\r\nExpect: 100-continue\r\nX-Tag: refused\r\nX-Allow: no\r\n"
        "Connection: close\r\n\r\n"
    )
    statuses, rest = exchange(site, head, None)
    assert statuses == [statuses[0]]
    assert statuses[0].startswith(b"HTTP/1.1 403")
    assert b'"refused": true' in rest
    assert "refused" in site.prechecks.read_text().split()


def test_an_asgi_application_reaches_the_files_through_its_scope(site: Site) -> None:
    body = form([("title", "t"), ("avatar", ("a.txt", "text/plain", b"from asgi"))])
    response = post(site, "/asgi/describe", body, {"Content-Type": FORM_TYPE})
    assert response.status == 200, response.body
    seen = json.loads(response.body)
    (upload,) = seen["uploads"]
    assert upload["field"] == "avatar"
    assert upload["content"] == "from asgi"
    assert "avatar.ref" in seen["body"]
    assert "from asgi" not in seen["body"]


@pytest.mark.parametrize("framework", ["flask", "starlette", "django"])
def test_the_framework_recipes_give_the_frameworks_own_upload_objects(
    site: Site, framework: str
) -> None:
    pytest.importorskip(framework)
    body = form([("title", "hello"), ("avatar", ("a.txt", "text/plain", b"framework bytes"))])
    response = post(site, f"/{framework}/describe", body, {"Content-Type": FORM_TYPE})
    assert response.status == 200, response.body
    seen = json.loads(response.body)
    assert seen["files"] == {"avatar": ["a.txt", "text/plain", "framework bytes"]}
    assert seen["fields"]["title"] == "hello"
    assert seen["fields"]["avatar.ref"] == "1"


def test_a_route_without_uploads_passes_files_to_the_application(site: Site) -> None:
    body = form([("avatar", ("a.txt", "text/plain", b"inline"))])
    seen = json.loads(post(site, "/describe", body, {"Content-Type": FORM_TYPE}).body)
    assert seen["uploads"] == []
    assert "inline" in seen["body"]


def test_the_route_list_and_the_export_show_the_upload_keys(site: Site) -> None:
    listed = site.daemon.muructl("route", "list").stdout
    assert "[uploads multipart]" in listed
    assert "[uploads multipart, precheck]" in listed
    assert "[max body 20MB]" in listed

    exported = site.daemon.muructl("config", "export").stdout
    assert 'upload = "raw"' in exported
    assert "precheck = true" in exported
    assert 'spool_quota = "64KB"' in exported


def test_apply_refuses_upload_keys_that_do_not_fit(site: Site) -> None:
    config = site.base / "bad.toml"
    config.write_text(
        f"""[app.uploads.release.r1]
source = '{site.base / "project"}'
python = '{sys.executable}'
processes = 1
env = {{ PRECHECKS = '{site.prechecks}' }}

[host.site]
names = ['site.test']
listeners = ['127.0.0.1:{site.daemon.port}']
default = true

[[route]]
host = 'site'
path = '/'
app = 'uploads'
precheck = true

[[route]]
host = 'site'
path = '/files/'
static = '{site.base}'
upload = 'raw'
"""
    )
    completed = site.daemon.muructl("apply", "-f", str(config), check=False)
    assert completed.returncode != 0
    assert "precheck needs upload" in completed.stdout
    assert "upload needs an application" in completed.stdout
