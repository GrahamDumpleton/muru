"""Unit tests for gRPC mounts (design specification section 6.13): the server
registration functions are given, the dispatcher that runs calls, and the
metadata and status encodings.
"""

from __future__ import annotations

import argparse
import asyncio
import os
import sys
from typing import Any

import pytest

from muru import host
from muru._context import Context

# Section 6.13: where grpcio cannot be installed, a free-threaded build,
# gRPC mounts are unavailable. Anywhere else under CI it must be there.

try:
    import grpc

    from muru import _grpc
except ImportError:
    if os.environ.get("CI") and getattr(sys, "_is_gil_enabled", lambda: True)():
        raise

    pytest.skip(
        "gRPC mounts need grpcio, which this Python cannot install (section 6.13)",
        allow_module_level=True,
    )


def echo(request: bytes, context: Any) -> bytes:
    context.set_trailing_metadata((("x-seen", "yes"),))
    return request.upper()


def handlers() -> dict[str, Any]:
    return {
        "Echo": grpc.unary_unary_rpc_method_handler(echo),
        "Stream": grpc.stream_stream_rpc_method_handler(lambda requests, context: requests),
    }


def test_the_server_lists_what_was_registered() -> None:
    server = _grpc.Server()
    server.add_generic_rpc_handlers((grpc.method_handlers_generic_handler("demo.Svc", handlers()),))
    server.add_registered_method_handlers("demo.Svc", handlers())
    assert server.services == ["demo.Svc"]
    assert sorted(server.listing("api")) == [
        ("/demo.Svc/Echo", "api", False, False),
        ("/demo.Svc/Stream", "api", True, True),
    ]

    # A generic handler whose methods cannot be listed stands for its
    # whole service.

    class Opaque(grpc.GenericRpcHandler):
        def service_name(self) -> str:
            return "demo.Other"

        def service(self, details: Any) -> Any:
            return None

    server.add_generic_rpc_handlers((Opaque(),))
    assert ("/demo.Other/", "api", True, True) in server.listing("api")


def test_a_registration_that_adds_nothing_is_refused() -> None:
    with pytest.raises(RuntimeError, match="no gRPC services"):
        _grpc.register(lambda server: None)


def test_a_grpc_mount_takes_no_path() -> None:
    context = Context(app="svc", release="dev", process_model="async")
    mount = host.Mount.parse("api==svc:register=grpc", context, 4)
    assert (mount.name, mount.path, mount.release.interface) == ("api", "", "grpc")

    for text in ("api=/api=svc:register=grpc", "api==svc:app", "api==svc:app=asgi"):
        with pytest.raises(argparse.ArgumentTypeError):
            host.Mount.parse(text, context, 4)

    assert host.resolve_interface("svc:register", lambda server: None, "grpc") == "grpc"
    assert host.process_model(4, "grpc") == "async"


def test_metadata_and_messages_follow_the_wire_formats() -> None:
    headers = [
        (b"content-type", b"application/grpc"),
        (b"grpc-timeout", b"5S"),
        (b"x-name", b"value"),
        (b"x-data-bin", b"AAE"),
    ]
    assert _grpc.decode_metadata(headers) == (("x-name", "value"), ("x-data-bin", b"\x00\x01"))
    assert _grpc.encode_metadata([("X-Name", "value"), ("x-data-bin", b"\x00\x01")]) == [
        (b"x-name", b"value"),
        (b"x-data-bin", b"AAE"),
    ]
    assert _grpc.percent_encode("50% off\n") == b"50%25 off%0A"
    assert _grpc.describe_peer(("192.0.2.7", 51234)) == "ipv4:192.0.2.7:51234"
    assert _grpc.describe_peer(("::1", 8)) == "ipv6:[::1]:8"
    assert _grpc.status(5) is grpc.StatusCode.NOT_FOUND


def run_call(
    server: _grpc.Server, path: str, messages: list[bytes], kind: str = "grpc"
) -> list[dict[str, Any]]:
    """Run one call through a dispatcher and return what it sent."""

    events = [{"type": "grpc.message", "message": message} for message in messages]
    events.append({"type": "grpc.end"})
    sent: list[dict[str, Any]] = []

    async def receive() -> dict[str, Any]:
        if events:
            return events.pop(0)

        await asyncio.sleep(3600)
        return {"type": "grpc.disconnect"}

    async def send(message: dict[str, Any]) -> None:
        sent.append(message)

    scope = {"type": kind, "path": path, "headers": [], "client": ("192.0.2.7", 5)}
    asyncio.run(_grpc.Dispatcher(server, 2)(scope, receive, send))
    return sent


def test_the_dispatcher_runs_calls_and_ends_them_with_a_status() -> None:
    server = _grpc.Server()
    server.add_registered_method_handlers("demo.Svc", handlers())

    assert run_call(server, "/demo.Svc/Echo", [b"hi"]) == [
        {"type": "grpc.message", "message": b"HI"},
        {
            "type": "grpc.finish",
            "trailers": [(b"grpc-status", b"0"), (b"x-seen", b"yes")],
        },
    ]

    streamed = run_call(server, "/demo.Svc/Stream", [b"a", b"b"])
    assert [event.get("message") for event in streamed[:-1]] == [b"a", b"b"]

    (missing,) = run_call(server, "/demo.Svc/Nope", [])
    assert missing["trailers"][0] == (b"grpc-status", b"12")

    start, _ = run_call(server, "/", [], kind="http")
    assert start["status"] == 404
