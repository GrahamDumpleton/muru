"""A WSGI application at / beside a gRPC mount (section 6.13).

The Greeter service mixes synchronous and asynchronous handlers across the
four kinds of call. greeter_pb2 and greeter_pb2_grpc are generated from
greeter.proto by the tests.
"""

import asyncio
import os
import time

import greeter_pb2
import greeter_pb2_grpc
import grpc


def wsgi_app(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"plain HTTP\n"]


class Greeter(greeter_pb2_grpc.GreeterServicer):
    def SayHello(self, request, context):
        if request.name == "fail":
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, "fail is not a name")

        if request.name == "boom":
            raise ValueError("boom")

        if request.name == "slow":
            time.sleep(3)

        greeting = dict(context.invocation_metadata()).get("x-greeting", "Hello")
        context.send_initial_metadata((("x-initial", "yes"),))
        context.set_trailing_metadata((("x-trailing", "done"), ("x-data-bin", b"\x00\x01")))
        return greeter_pb2.Reply(message=f"{greeting} {request.name}")

    async def Whoami(self, request, context):
        remaining = context.time_remaining()
        return greeter_pb2.Reply(message=f"{os.getpid()} {context.peer()} {remaining is not None}")

    async def Count(self, request, context):
        for number in range(1, request.count + 1):
            await asyncio.sleep(0.01)
            yield greeter_pb2.Reply(message=str(number))

    def CountSync(self, request, context):
        for number in range(1, request.count + 1):
            yield greeter_pb2.Reply(message=str(number))

    def Total(self, request_iterator, context):
        names = [request.name for request in request_iterator]
        return greeter_pb2.Reply(message="+".join(names))

    async def Chat(self, request_iterator, context):
        async for request in request_iterator:
            yield greeter_pb2.Reply(message=f"echo:{request.name}")


def register(server):
    greeter_pb2_grpc.add_GreeterServicer_to_server(Greeter(), server)
