"""gRPC service mounts (design specification section 6.13).

A release serves gRPC services from the same workers as its WSGI
application: grpcio servicers registered by a function, matched by
content type and service name, all four kinds of call over h2c and h2,
gRPC-Web translated at the edge, the health service, reflection, and
calls balanced across the workers one by one. On a Python that cannot
install grpcio, gRPC mounts are unavailable and these tests are skipped.
"""

from __future__ import annotations

import base64
import http.client
import os
import shutil
import struct
import subprocess
import sys
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest
from support import server as support
from support.daemon import start_daemon

# Section 6.13: where grpcio cannot be installed, a free-threaded build,
# gRPC mounts are unavailable. Anywhere else under CI it must be there.

try:
    import grpc
    import grpc_tools  # noqa: F401  (the tests generate their stubs with it)
except ImportError:
    if os.environ.get("CI") and getattr(sys, "_is_gil_enabled", lambda: True)():
        raise

    pytest.skip(
        "gRPC mounts need grpcio, which this Python cannot install (section 6.13)",
        allow_module_level=True,
    )

FIXTURE = support.ROOT / "tests" / "fixtures" / "projects" / "grpcsvc"


@dataclass
class Service:
    server: support.Server
    project: Path
    cache: Path
    pb: ModuleType
    rpc: ModuleType

    def channel(self) -> Any:
        return grpc.insecure_channel(f"127.0.0.1:{self.server.port}")

    def stub(self) -> Any:
        return self.rpc.GreeterStub(self.channel())


def generate(project: Path) -> tuple[ModuleType, ModuleType]:
    """Generate the fixture's stubs with grpcio-tools, and import them here too."""

    subprocess.run(
        [
            sys.executable,
            "-m",
            "grpc_tools.protoc",
            f"-I{project}",
            f"--python_out={project}",
            f"--grpc_python_out={project}",
            str(project / "greeter.proto"),
        ],
        check=True,
    )
    sys.path.insert(0, str(project))
    import greeter_pb2
    import greeter_pb2_grpc

    return greeter_pb2, greeter_pb2_grpc


@pytest.fixture(scope="module")
def service(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Service]:
    base = tmp_path_factory.mktemp("grpc")
    project = base / "project"
    shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
    pb, rpc = generate(project)
    cache = base / "cache"
    instance = support.start(
        str(project),
        workers=2,
        threads=2,
        chdir=None,
        args=["--https"],
        env={"MURU_CACHE_DIR": str(cache), "CAROOT": str(base / "caroot")},
        https=True,
    )

    try:
        yield Service(instance, project, cache, pb, rpc)
    finally:
        instance.stop()
        sys.path.remove(str(project))


def four_kinds_of_call(stub: Any, pb: ModuleType) -> None:
    """Make a unary, a server streaming, a client streaming and a bidirectional call."""

    reply, call = stub.SayHello.with_call(
        pb.Hello(name="Ada"), metadata=(("x-greeting", "Hi"),), timeout=10
    )
    assert reply.message == "Hi Ada"
    assert ("x-initial", "yes") in call.initial_metadata()
    assert ("x-trailing", "done") in call.trailing_metadata()
    assert ("x-data-bin", b"\x00\x01") in call.trailing_metadata()

    assert [reply.message for reply in stub.Count(pb.Hello(count=3), timeout=10)] == ["1", "2", "3"]
    assert [reply.message for reply in stub.CountSync(pb.Hello(count=2), timeout=10)] == ["1", "2"]

    names = iter([pb.Hello(name=name) for name in ("a", "b", "c")])
    assert stub.Total(names, timeout=10).message == "a+b+c"

    chat = stub.Chat(iter([pb.Hello(name="x"), pb.Hello(name="y")]), timeout=10)
    assert [reply.message for reply in chat] == ["echo:x", "echo:y"]


def test_the_four_kinds_of_call_over_h2c(service: Service) -> None:
    four_kinds_of_call(service.stub(), service.pb)


def test_the_four_kinds_of_call_over_h2(service: Service) -> None:
    certificate = (service.cache / "certs" / "dev.pem").read_bytes()
    credentials = grpc.ssl_channel_credentials(root_certificates=certificate)
    channel = grpc.secure_channel(
        f"127.0.0.1:{service.server.https_port}",
        credentials,
        options=(("grpc.ssl_target_name_override", "localhost"),),
    )
    four_kinds_of_call(service.rpc.GreeterStub(channel), service.pb)


def test_statuses_from_the_handler_and_the_edge(service: Service) -> None:
    stub = service.stub()

    with pytest.raises(grpc.RpcError) as aborted:
        stub.SayHello(service.pb.Hello(name="fail"), timeout=10)

    assert aborted.value.code() == grpc.StatusCode.INVALID_ARGUMENT
    assert aborted.value.details() == "fail is not a name"

    with pytest.raises(grpc.RpcError) as raised:
        stub.SayHello(service.pb.Hello(name="boom"), timeout=10)

    assert raised.value.code() == grpc.StatusCode.UNKNOWN
    assert raised.value.details() == "Exception calling application: boom"

    # The edge ends a call at its deadline, and answers a method nobody
    # registered.

    with pytest.raises(grpc.RpcError) as late:
        stub.SayHello(service.pb.Hello(name="slow"), timeout=0.5)

    assert late.value.code() == grpc.StatusCode.DEADLINE_EXCEEDED

    with pytest.raises(grpc.RpcError) as missing:
        service.channel().unary_unary("/demo.Greeter/Nope")(b"", timeout=10)

    assert missing.value.code() == grpc.StatusCode.UNIMPLEMENTED
    assert missing.value.details() == "Method not found: /demo.Greeter/Nope"


def test_calls_are_balanced_across_the_workers(service: Service) -> None:
    stub = service.stub()
    replies = [stub.Whoami(service.pb.Hello(), timeout=10).message for _ in range(8)]
    pids = {reply.split()[0] for reply in replies}
    assert len(pids) == 2, "one connection's calls go to both workers"

    # The context knows the client, and the deadline the client set.

    _, peer, has_deadline = replies[0].split()
    assert peer.startswith("ipv4:127.0.0.1:")
    assert has_deadline == "True"


def test_plain_http_still_reaches_the_wsgi_application(service: Service) -> None:
    response = service.server.get("/demo.Greeter/SayHello")
    assert (response.status, response.body) == (200, b"plain HTTP\n")


def encode_string(field: int, text: str) -> bytes:
    data = text.encode()
    return bytes([field << 3 | 2, len(data)]) + data


def test_the_health_service_answers_from_the_edge(service: Service) -> None:
    check = service.channel().unary_unary("/grpc.health.v1.Health/Check")
    assert check(b"", timeout=10) == b"\x08\x01", "SERVING"
    assert check(encode_string(1, "demo.Greeter"), timeout=10) == b"\x08\x01"

    with pytest.raises(grpc.RpcError) as unknown:
        check(encode_string(1, "nope.Service"), timeout=10)

    assert unknown.value.code() == grpc.StatusCode.NOT_FOUND

    watch = service.channel().unary_stream("/grpc.health.v1.Health/Watch")
    statuses = watch(b"", timeout=10)
    assert next(statuses) == b"\x08\x01"
    statuses.cancel()


def test_reflection_lists_and_describes_the_services(service: Service) -> None:
    from grpc_reflection.v1alpha import reflection_pb2, reflection_pb2_grpc

    stub = reflection_pb2_grpc.ServerReflectionStub(service.channel())
    requests = [
        reflection_pb2.ServerReflectionRequest(list_services=""),
        reflection_pb2.ServerReflectionRequest(file_containing_symbol="demo.Greeter"),
    ]
    listed, described = list(stub.ServerReflectionInfo(iter(requests), timeout=10))
    names = {entry.name for entry in listed.list_services_response.service}
    assert names == {"demo.Greeter", "grpc.reflection.v1alpha.ServerReflection"}
    assert described.file_descriptor_response.file_descriptor_proto


def web_frames(data: bytes) -> list[tuple[int, bytes]]:
    frames = []

    while data:
        flags, length = struct.unpack(">BI", data[:5])
        frames.append((flags, data[5 : 5 + length]))
        data = data[5 + length :]

    return frames


def web_call(
    service: Service, path: str, messages: list[bytes], content_type: str
) -> tuple[int, str, list[tuple[int, bytes]]]:
    body = b"".join(struct.pack(">BI", 0, len(message)) + message for message in messages)
    text = content_type.startswith("application/grpc-web-text")

    if text:
        body = base64.b64encode(body)

    connection = http.client.HTTPConnection("127.0.0.1", service.server.port, timeout=30)

    try:
        connection.request("POST", path, body=body, headers={"Content-Type": content_type})
        response = connection.getresponse()
        data = response.read()
    finally:
        connection.close()

    # Each base64 segment of a grpc-web-text response is padded on its
    # own, and every group of four characters decodes alone.

    if text:
        data = b"".join(base64.b64decode(data[at : at + 4]) for at in range(0, len(data), 4))

    return response.status, response.getheader("Content-Type") or "", web_frames(data)


@pytest.mark.parametrize(
    "content_type", ["application/grpc-web+proto", "application/grpc-web-text"]
)
def test_grpc_web_is_translated_at_the_edge(service: Service, content_type: str) -> None:
    pb = service.pb
    hello = pb.Hello(name="web").SerializeToString()
    status, returned, frames = web_call(service, "/demo.Greeter/SayHello", [hello], content_type)
    assert status == 200 and returned.startswith(content_type.split("+")[0])
    (flags, message), (trailer_flags, trailers) = frames
    assert flags == 0 and pb.Reply.FromString(message).message == "Hello web"
    assert trailer_flags == 0x80
    assert b"grpc-status: 0\r\n" in trailers and b"x-trailing: done\r\n" in trailers

    def replies(path: str, messages: list[bytes]) -> list[str]:
        _, _, frames = web_call(service, path, messages, content_type)
        assert frames[-1][0] == 0x80 and b"grpc-status: 0\r\n" in frames[-1][1]
        return [pb.Reply.FromString(data).message for flags, data in frames if flags == 0]

    # The streaming calls too, with the whole request in one body.

    count = pb.Hello(count=3).SerializeToString()
    assert replies("/demo.Greeter/Count", [count]) == ["1", "2", "3"]
    names = [pb.Hello(name=name).SerializeToString() for name in ("p", "q")]
    assert replies("/demo.Greeter/Total", names) == ["p+q"]
    assert replies("/demo.Greeter/Chat", names) == ["echo:p", "echo:q"]


@pytest.fixture(scope="module")
def grpc_only(service: Service) -> Iterator[support.Server]:
    instance = support.start(
        "grpcsvc:register",
        workers=1,
        threads=2,
        chdir=service.project,
        args=["--interface", "grpc"],
    )

    try:
        yield instance
    finally:
        instance.stop()


def test_a_grpc_only_release_answers_plain_http_with_404(
    service: Service, grpc_only: support.Server
) -> None:
    stub = service.rpc.GreeterStub(grpc.insecure_channel(f"127.0.0.1:{grpc_only.port}"))
    assert stub.SayHello(service.pb.Hello(name="only"), timeout=10).message == "Hello only"
    assert grpc_only.get("/").status == 404


def test_a_cancelled_stream_frees_its_worker(service: Service) -> None:
    stub = service.stub()
    pb = service.pb

    # More streams than the two workers' loops would hold if cancelled
    # calls lingered, each cancelled after its first reply.

    for _ in range(6):
        stream = stub.Count(pb.Hello(count=10_000), timeout=30)
        assert next(stream).message == "1"
        stream.cancel()

    assert stub.SayHello(pb.Hello(name="after"), timeout=10).message == "Hello after"


def test_the_daemon_serves_a_grpc_mount_from_an_applied_file(
    service: Service, tmp_path: Path
) -> None:
    daemon = start_daemon(tmp_path)

    try:
        config = tmp_path / "svc.toml"
        release = f"""[app.svc.release.r1]
source = '{service.project}'
python = '{sys.executable}'
entrypoint = 'grpcsvc:wsgi_app'
processes = 1
reflection = true

[app.svc.release.r1.mounts.api]
interface = 'grpc'
entrypoint = 'grpcsvc:register'
"""
        routes = f"""
[host.site]
names = ['site.test']
listeners = ['127.0.0.1:{daemon.port}']
default = true

[[route]]
name = 'root'
host = 'site'
path = '/'
app = 'svc'
"""

        # A gRPC mount is matched by service name, so a path is refused.

        config.write_text(
            release.replace("interface = 'grpc'", "interface = 'grpc'\npath = '/api'") + routes
        )
        refused = daemon.muructl("apply", "-f", str(config), check=False)
        assert refused.returncode != 0
        assert "a gRPC mount has no path" in refused.stdout + refused.stderr

        config.write_text(release + routes)
        daemon.muructl("apply", "-f", str(config))
        daemon.muructl("release", "wait", "svc@r1", "--timeout", "60")
        stub = service.rpc.GreeterStub(grpc.insecure_channel(f"127.0.0.1:{daemon.port}"))
        assert stub.SayHello(service.pb.Hello(name="daemon"), timeout=10).message == "Hello daemon"

        exported = daemon.muructl("config", "export").stdout
        assert "reflection = true" in exported
        assert 'interface = "grpc"' in exported
    finally:
        daemon.stop()
