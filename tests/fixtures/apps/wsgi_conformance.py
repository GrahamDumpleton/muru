"""A WSGI application exercising the PEP 3333 contract, for the conformance suite.

Each area has a path prefix and the cases under it. Responses carry exact
values so the tests compare rather than search, and many end with ``;end``
so a trailing newline in user data cannot be lost. The cases follow the
mod_wsgi integration tests for the same areas, which cover behaviour the
specification leaves to the server as much as behaviour it requires.
"""

from __future__ import annotations

import ctypes
import io
import os
import sys
import tempfile
import threading
import time
from collections.abc import Callable, Iterable, Iterator
from typing import Any
from urllib.parse import parse_qs

StartResponse = Callable[..., Any]
Environ = dict[str, Any]

_PyThreadState_Get = ctypes.pythonapi.PyThreadState_Get
_PyThreadState_Get.restype = ctypes.c_void_p

FILE_CONTENT = b"file_wrapper test: SUCCESS\n" * 100
_file = tempfile.NamedTemporaryFile(delete=False)
_file.write(FILE_CONTENT)
_file.close()
FILE_PATH = _file.name


def application(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    segments = environ["PATH_INFO"].lstrip("/").split("/")
    area = segments[0] if segments else ""
    case = segments[1] if len(segments) > 1 else ""
    handlers = AREAS.get(area)
    handler = handlers.get(case) if handlers else None
    if handler is None:
        start_response("404 Not Found", [("Content-Type", "text/plain")])
        return [b"unknown test path\n"]
    return handler(environ, start_response)


def respond(start_response: StartResponse, body: bytes, status: str = "200 OK") -> list[bytes]:
    payload = body + b";end"
    start_response(
        status,
        [("Content-Type", "text/plain"), ("Content-Length", str(len(payload)))],
    )
    return [payload]


def query(environ: Environ) -> dict[str, str]:
    return {key: values[0] for key, values in parse_qs(environ["QUERY_STRING"]).items()}


def error_body(start_response: StartResponse, error: BaseException) -> list[bytes]:
    return respond(start_response, f"{type(error).__name__}:{error}".encode())


# ----- start_response -----


def sr_write(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    write = start_response("200 OK", [("Content-Type", "text/plain")])
    write(b"via-write-")
    return [b"via-iter"]


def sr_write_before_return(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    write = start_response("200 OK", [("Content-Type", "text/plain")])
    write(b"pre-")
    write(b"mid-")
    return [b"post1-", b"post2"]


def sr_non_tuple_exc_info(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    try:
        start_response("200 OK", [("Content-Type", "text/plain")], "not-a-tuple")
    except (TypeError, RuntimeError) as error:
        return error_body(start_response, error)
    return respond(start_response, b"no-error-raised")


def sr_exc_info_after_headers(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    write = start_response("200 OK", [("Content-Type", "text/plain")])
    write(b"headers-sent-")
    try:
        raise RuntimeError("intentional-exc-info")
    except RuntimeError:
        try:
            start_response(
                "500 Internal Server Error", [("Content-Type", "text/plain")], sys.exc_info()
            )
        except RuntimeError as error:
            write(f"reraised:{error}".encode())
            return []
    return [b"not-reraised"]


def sr_exc_info_before_headers(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain"), ("X-Before", "first")])
    try:
        raise RuntimeError("before-headers-sent")
    except RuntimeError:
        start_response(
            "418 I'm a teapot",
            [("Content-Type", "text/plain"), ("X-After", "second")],
            sys.exc_info(),
        )
    return [b"second-headers-win"]


def sr_double_before_output(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain"), ("X-First", "1")])
    start_response("201 Created", [("Content-Type", "text/plain"), ("X-Second", "2")])
    return [b"second-wins"]


def sr_double_after_output(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    write = start_response("200 OK", [("Content-Type", "text/plain")])
    write(b"pre-")
    try:
        start_response("500 Internal Server Error", [("Content-Type", "text/plain")])
    except RuntimeError as error:
        write(f"err:{error}".encode())
        return []
    return [b"no-error-raised"]


INVALID_STATUS = {
    "no-digits": "OK",
    "mixed": "20X OK",
    "four-digits": "2000 OK",
    "no-space": "200OK",
    "control-char": "200 OK\r\nX-Injected: yes",
    "empty": "",
    "not-str": b"200 OK",
}


def sr_invalid_status(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    status = INVALID_STATUS[query(environ)["case"]]
    try:
        start_response(status, [("Content-Type", "text/plain")])
    except (ValueError, TypeError) as error:
        return error_body(start_response, error)
    return respond(start_response, b"no-error-raised")


INVALID_HEADER: dict[str, list[Any]] = {
    "empty-name": [("", "value")],
    "space-name": [("X Foo", "value")],
    "control-name": [("X\tFoo", "value")],
    "cr-value": [("X-Foo", "line1\r\nline2")],
    "lf-value": [("X-Foo", "line1\nline2")],
    "non-string-name": [(42, "value")],
    "non-string-value": [("X-Foo", 42)],
    "non-latin1-value": [("X-Foo", "日本")],
    "not-a-tuple": ["X-Foo:value"],
    "wrong-tuple-size": [("a", "b", "c")],
    "bad-content-length": [("Content-Length", "not-a-number")],
}


def sr_invalid_header(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    extra = INVALID_HEADER[query(environ)["case"]]
    try:
        start_response("200 OK", [("Content-Type", "text/plain"), *extra])
    except (ValueError, TypeError, UnicodeEncodeError) as error:
        return error_body(start_response, error)
    return respond(start_response, b"no-error-raised")


INVALID_HEADERS_TYPE: dict[str, Any] = {
    "string": "Content-Type: text/plain",
    "tuple": (("Content-Type", "text/plain"),),
    "dict": {"Content-Type": "text/plain"},
}


def sr_invalid_headers_type(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    headers = INVALID_HEADERS_TYPE[query(environ)["case"]]
    try:
        start_response("200 OK", headers)
    except TypeError as error:
        return error_body(start_response, error)
    return respond(start_response, b"no-error-raised")


def sr_never_called(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    return [b"no start_response"]


def sr_status_reason_phrase(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    return respond(start_response, b"custom reason", status="299 Custom Reason Here")


# ----- response iteration -----


def rs_list(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"chunk1-", b"chunk2-", b"chunk3"]


def rs_tuple(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain")])
    return (b"tuple-a", b"tuple-b")


def rs_generator(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain")])

    def gen() -> Iterator[bytes]:
        yield b"gen-one-"
        yield b"gen-two-"
        yield b"gen-three"

    return gen()


def rs_empty_list(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain")])
    return []


def rs_empty_generator(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain")])

    def gen() -> Iterator[bytes]:
        return
        yield b""  # pragma: no cover

    return gen()


def rs_empty_chunks(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"", b"a", b"", b"b", b""]


def rs_iterable_with_close(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    errors = environ["wsgi.errors"]

    class Items:
        def __init__(self) -> None:
            self.items = [b"iter-a-", b"iter-b"]

        def __iter__(self) -> Items:
            return self

        def __next__(self) -> bytes:
            if not self.items:
                raise StopIteration
            return self.items.pop(0)

        def close(self) -> None:
            errors.write("MARKER_ITER_CLOSE\n")
            errors.flush()

    start_response("200 OK", [("Content-Type", "text/plain")])
    return Items()


def rs_generator_finally(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    errors = environ["wsgi.errors"]

    def gen() -> Iterator[bytes]:
        try:
            yield b"gen-fin-a-"
            yield b"gen-fin-b"
        finally:
            errors.write("MARKER_GEN_FINALLY\n")
            errors.flush()

    start_response("200 OK", [("Content-Type", "text/plain")])
    return gen()


def rs_close_via_generator_exit(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    errors = environ["wsgi.errors"]

    def gen() -> Iterator[Any]:
        try:
            yield b"first-ok"
            yield 12345
            yield b"never-reached"
        finally:
            errors.write("MARKER_CLOSE_GEXIT\n")
            errors.flush()

    start_response("200 OK", [("Content-Type", "text/plain")])
    return gen()


def rs_close_raises(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    class Items:
        def __iter__(self) -> Iterator[bytes]:
            return iter([b"before-close-error"])

        def close(self) -> None:
            raise RuntimeError("close-boom")

    start_response("200 OK", [("Content-Type", "text/plain")])
    return Items()


def rs_raise_first(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    def gen() -> Iterator[bytes]:
        raise RuntimeError("raise-first-boom")
        yield b""  # pragma: no cover

    start_response("200 OK", [("Content-Type", "text/plain")])
    return gen()


def rs_raise_midway(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    def gen() -> Iterator[bytes]:
        yield b"before-boom"
        raise RuntimeError("raise-midway-boom")

    start_response("200 OK", [("Content-Type", "text/plain")])
    return gen()


def rs_raise_with_finally(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    errors = environ["wsgi.errors"]

    def gen() -> Iterator[bytes]:
        try:
            yield b"before-fin-err"
            raise RuntimeError("raise-after-yield")
        finally:
            errors.write("MARKER_RAISE_FINALLY\n")
            errors.flush()

    start_response("200 OK", [("Content-Type", "text/plain")])
    return gen()


def rs_raise_before_start(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    raise RuntimeError("raise-before-start-boom")


def rs_raise_systemexit(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    raise SystemExit("raise-systemexit-msg")


def rs_raise_keyboardinterrupt(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    raise KeyboardInterrupt


def rs_non_bytes_first(environ: Environ, start_response: StartResponse) -> Iterable[Any]:
    start_response("200 OK", [("Content-Type", "text/plain")])
    return ["not-bytes-str"]


def rs_non_bytes_midway(environ: Environ, start_response: StartResponse) -> Iterable[Any]:
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"bytes-ok", 12345]


def rs_bytearray_item(environ: Environ, start_response: StartResponse) -> Iterable[Any]:
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [bytearray(b"bytearray-item")]


def rs_not_iterable(environ: Environ, start_response: StartResponse) -> Iterable[Any]:
    start_response("200 OK", [("Content-Type", "text/plain")])
    return 42  # type: ignore[return-value]


def rs_content_length_exact(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    content = b"exactly10!"
    start_response(
        "200 OK", [("Content-Type", "text/plain"), ("Content-Length", str(len(content)))]
    )
    return [content]


def rs_content_length_over(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "5")])
    return [b"this-is-longer-than-five"]


def rs_content_length_over_chunks(
    environ: Environ, start_response: StartResponse
) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "7")])
    return [b"abc", b"def", b"ghi", b"jkl"]


def rs_content_length_under(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "100")])
    return [b"only-ten!!"]


def rs_content_length_zero(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "0")])
    return []


def rs_many_chunks(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain")])

    def gen() -> Iterator[bytes]:
        for index in range(100):
            yield b"chunk%03d-" % index

    return gen()


def rs_slow_chunks(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain")])

    def gen() -> Iterator[bytes]:
        for index in range(3):
            yield b"slow%d-" % index
            time.sleep(0.05)

    return gen()


def rs_large(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    size = int(query(environ).get("size", "1048576"))
    start_response("200 OK", [("Content-Type", "application/octet-stream")])
    return [bytes(range(256)) * (size // 256) + bytes(range(size % 256))]


def rs_no_content_type(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [])
    return [b"no content type"]


def rs_status_204(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("204 No Content", [])
    return []


def rs_status_304(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("304 Not Modified", [("ETag", '"abc"')])
    return []


# ----- wsgi.input -----


def in_read_all(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    return respond(start_response, b"data=" + environ["wsgi.input"].read())


def in_read_sized(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    stream = environ["wsgi.input"]
    first = stream.read(10)
    rest = stream.read()
    return respond(start_response, b"first=" + first + b",rest=" + rest)


def in_read_chunks(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    stream = environ["wsgi.input"]
    chunks = []
    while True:
        chunk = stream.read(7)
        if not chunk:
            break
        chunks.append(chunk)
    return respond(start_response, b"chunks=%d,data=%s" % (len(chunks), b"".join(chunks)))


def in_read_zero(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    stream = environ["wsgi.input"]
    zero = stream.read(0)
    rest = stream.read()
    return respond(start_response, b"zero_len=%d,rest=%s" % (len(zero), rest))


def in_read_past_eof(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    stream = environ["wsgi.input"]
    first = stream.read()
    second = stream.read()
    third = stream.read(5)
    return respond(
        start_response,
        b"first_len=%d,second_len=%d,third_len=%d" % (len(first), len(second), len(third)),
    )


def in_read_more_than_body(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    data = environ["wsgi.input"].read(1000000)
    return respond(start_response, b"data=" + data)


def in_readline(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    stream = environ["wsgi.input"]
    lines = []
    while True:
        line = stream.readline()
        if not line:
            break
        lines.append(line.rstrip(b"\n"))
    return respond(start_response, b"count=%d,lines=%s" % (len(lines), b"|".join(lines)))


def in_readline_sized(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    stream = environ["wsgi.input"]
    first = stream.readline(5)
    second = stream.readline(5)
    rest = stream.read()
    return respond(start_response, b"first=" + first + b",second=" + second + b",rest=" + rest)


def in_readlines(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    lines = environ["wsgi.input"].readlines()
    return respond(
        start_response,
        b"count=%d,lines=%s" % (len(lines), b"|".join(line.rstrip(b"\n") for line in lines)),
    )


def in_readlines_hint(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    lines = environ["wsgi.input"].readlines(1)
    return respond(
        start_response,
        b"count=%d,lines=%s" % (len(lines), b"|".join(line.rstrip(b"\n") for line in lines)),
    )


def in_iterate(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    lines = [line.rstrip(b"\n") for line in environ["wsgi.input"]]
    return respond(start_response, b"count=%d,lines=%s" % (len(lines), b"|".join(lines)))


def in_digest(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    import hashlib

    stream = environ["wsgi.input"]
    digest = hashlib.sha256()
    total = 0
    while True:
        chunk = stream.read(8192)
        if not chunk:
            break
        digest.update(chunk)
        total += len(chunk)
    return respond(start_response, b"len=%d sha256=%s" % (total, digest.hexdigest().encode()))


def in_unread(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    return respond(start_response, b"ignored")


def in_read_after_response(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    # Write the response first, then read the body: both directions are
    # independent, so this must not deadlock whatever the body size.

    write = start_response("200 OK", [("Content-Type", "text/plain")])
    write(b"started-")
    data = environ["wsgi.input"].read()
    return [b"read=%d" % len(data)]


def in_content_length_keys(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    body = environ["wsgi.input"].read()
    present = "CONTENT_LENGTH" in environ
    value = environ.get("CONTENT_LENGTH", "MISSING")
    terminated = environ.get("wsgi.input_terminated")
    return respond(
        start_response,
        f"present={present},value={value},terminated={terminated},len={len(body)}".encode(),
    )


# ----- environ -----


def env_has(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    key = query(environ)["key"]
    return respond(start_response, b"present=YES" if key in environ else b"present=NO")


def env_get(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    key = query(environ)["key"]
    value = str(environ[key]) if key in environ else "MISSING"
    return respond(start_response, f"value={value}".encode("utf-8", "surrogateescape"))


def env_type(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    key = query(environ)["key"]
    if key not in environ:
        return respond(start_response, b"type=MISSING")
    value = environ[key]
    kind = value if isinstance(value, type) else type(value)
    return respond(start_response, f"type={kind.__module__}.{kind.__name__}".encode())


def env_all_http(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    keys = sorted(key for key in environ if key.startswith("HTTP_"))
    return respond(start_response, ("keys=" + ",".join(keys)).encode())


def env_path_bytes(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    # PATH_INFO as the latin-1 bytes PEP 3333 says it carries.

    raw = environ["PATH_INFO"].encode("latin-1")
    return respond(start_response, b"path=" + raw)


# ----- headers -----


def hd_custom(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain"), ("X-Test-Custom", "hello-world")])
    return [b"ok"]


def hd_multiple(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response(
        "200 OK",
        [
            ("Content-Type", "text/plain"),
            ("X-Test-First", "one"),
            ("X-Test-Second", "two"),
            ("X-Test-Third", "three"),
        ],
    )
    return [b"ok"]


def hd_repeated(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response(
        "200 OK",
        [
            ("Content-Type", "text/plain"),
            ("X-Test-Repeated", "first-value"),
            ("X-Test-Repeated", "second-value"),
        ],
    )
    return [b"ok"]


def hd_set_cookie(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response(
        "200 OK",
        [
            ("Content-Type", "text/plain"),
            ("Set-Cookie", "a=1; Path=/"),
            ("Set-Cookie", "b=2; Path=/"),
        ],
    )
    return [b"ok"]


def hd_www_authenticate(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response(
        "401 Unauthorized",
        [("Content-Type", "text/plain"), ("WWW-Authenticate", 'Basic realm="test"')],
    )
    return [b"auth required"]


def hd_many(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    headers = [("Content-Type", "text/plain")]
    headers.extend((f"X-Many-{index:02d}", f"value-{index}") for index in range(1, 26))
    start_response("200 OK", headers)
    return [b"ok"]


def hd_latin1_value(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain"), ("X-Latin1", "café")])
    return [b"ok"]


def hd_mixed_case(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    start_response("200 OK", [("content-TYPE", "text/plain"), ("x-MiXeD", "value")])
    return [b"ok"]


# ----- methods -----


def me_echo(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    method = environ["REQUEST_METHOD"]
    body = f"method={method};end".encode()
    start_response(
        "200 OK",
        [
            ("Content-Type", "text/plain"),
            ("Content-Length", str(len(body))),
            ("X-Method-Seen", method),
        ],
    )
    return [body]


# ----- file_wrapper -----


def fw_basic(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    handle = open(FILE_PATH, "rb")
    start_response("200 OK", [("Content-Type", "text/plain")])
    return environ["wsgi.file_wrapper"](handle, 8192)


def fw_with_content_length(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    handle = open(FILE_PATH, "rb")
    start_response(
        "200 OK", [("Content-Type", "text/plain"), ("Content-Length", str(len(FILE_CONTENT)))]
    )
    return environ["wsgi.file_wrapper"](handle, 8192)


def fw_partial(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    handle = open(FILE_PATH, "rb")
    first_line = b"file_wrapper test: SUCCESS\n"
    handle.seek(len(first_line))
    remaining = len(FILE_CONTENT) - len(first_line)
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", str(remaining))])
    return environ["wsgi.file_wrapper"](handle, 8192)


def fw_range(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    handle = open(FILE_PATH, "rb")
    handle.seek(100)
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", "50")])
    return environ["wsgi.file_wrapper"](handle, 8192)


def fw_iterable_fallback(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    content = b"file_wrapper iterable fallback: SUCCESS"
    start_response(
        "200 OK", [("Content-Type", "text/plain"), ("Content-Length", str(len(content)))]
    )
    return environ["wsgi.file_wrapper"](io.BytesIO(content), 8192)


def fw_small_blocks(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    content = b"0123456789" * 10
    start_response("200 OK", [("Content-Type", "text/plain")])
    return environ["wsgi.file_wrapper"](io.BytesIO(content), 7)


def fw_wrapped_in_list(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    # Middleware often wraps the iterable; the wrapper must still iterate.

    handle = open(FILE_PATH, "rb")
    start_response("200 OK", [("Content-Type", "text/plain")])
    return list(environ["wsgi.file_wrapper"](handle, 8192))


class ClosingFile(io.BytesIO):
    """A file-like object that records close() in wsgi.errors."""

    def __init__(self, data: bytes, errors: Any) -> None:
        super().__init__(data)
        self.errors = errors

    def close(self) -> None:
        self.errors.write("MARKER_FILE_CLOSED\n")
        self.errors.flush()
        super().close()


def fw_closes_file(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    handle = ClosingFile(b"closed after sending", environ["wsgi.errors"])
    start_response("200 OK", [("Content-Type", "text/plain")])
    return environ["wsgi.file_wrapper"](handle, 8192)


# ----- wsgi.errors and standard streams -----


def er_write(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    environ["wsgi.errors"].write("MARKER_ERRORS_WRITE\n")
    environ["wsgi.errors"].flush()
    return respond(start_response, b"written")


def er_writelines(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    environ["wsgi.errors"].writelines(["MARKER_ERRORS_LINE1\n", "MARKER_ERRORS_LINE2\n"])
    environ["wsgi.errors"].flush()
    return respond(start_response, b"written")


def er_stdout(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    print("MARKER_STDOUT", flush=True)
    return respond(start_response, b"written")


def er_stderr(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    print("MARKER_STDERR", file=sys.stderr, flush=True)
    return respond(start_response, b"written")


# ----- threads -----


def th_identity(environ: Environ, start_response: StartResponse) -> Iterable[bytes]:
    time.sleep(float(query(environ).get("sleep", "0.02")))
    body = f"pid={os.getpid()},native={threading.get_native_id()},tstate={_PyThreadState_Get():#x}"
    return respond(start_response, body.encode())


AREAS: dict[str, dict[str, Callable[[Environ, StartResponse], Iterable[Any]]]] = {
    "start-response": {
        "write": sr_write,
        "write-before-return": sr_write_before_return,
        "non-tuple-exc-info": sr_non_tuple_exc_info,
        "exc-info-after-headers": sr_exc_info_after_headers,
        "exc-info-before-headers": sr_exc_info_before_headers,
        "double-before-output": sr_double_before_output,
        "double-after-output": sr_double_after_output,
        "invalid-status": sr_invalid_status,
        "invalid-header": sr_invalid_header,
        "invalid-headers-type": sr_invalid_headers_type,
        "never-called": sr_never_called,
        "status-reason-phrase": sr_status_reason_phrase,
    },
    "response": {
        "list": rs_list,
        "tuple": rs_tuple,
        "generator": rs_generator,
        "empty-list": rs_empty_list,
        "empty-generator": rs_empty_generator,
        "empty-chunks": rs_empty_chunks,
        "iterable-with-close": rs_iterable_with_close,
        "generator-finally": rs_generator_finally,
        "close-via-generator-exit": rs_close_via_generator_exit,
        "close-raises": rs_close_raises,
        "raise-first": rs_raise_first,
        "raise-midway": rs_raise_midway,
        "raise-with-finally": rs_raise_with_finally,
        "raise-before-start": rs_raise_before_start,
        "raise-systemexit": rs_raise_systemexit,
        "raise-keyboardinterrupt": rs_raise_keyboardinterrupt,
        "non-bytes-first": rs_non_bytes_first,
        "non-bytes-midway": rs_non_bytes_midway,
        "bytearray-item": rs_bytearray_item,
        "not-iterable": rs_not_iterable,
        "content-length-exact": rs_content_length_exact,
        "content-length-over": rs_content_length_over,
        "content-length-over-chunks": rs_content_length_over_chunks,
        "content-length-under": rs_content_length_under,
        "content-length-zero": rs_content_length_zero,
        "many-chunks": rs_many_chunks,
        "slow-chunks": rs_slow_chunks,
        "large": rs_large,
        "no-content-type": rs_no_content_type,
        "status-204": rs_status_204,
        "status-304": rs_status_304,
    },
    "input": {
        "read-all": in_read_all,
        "read-sized": in_read_sized,
        "read-chunks": in_read_chunks,
        "read-zero": in_read_zero,
        "read-past-eof": in_read_past_eof,
        "read-more-than-body": in_read_more_than_body,
        "readline": in_readline,
        "readline-sized": in_readline_sized,
        "readlines": in_readlines,
        "readlines-hint": in_readlines_hint,
        "iterate": in_iterate,
        "digest": in_digest,
        "unread": in_unread,
        "read-after-response": in_read_after_response,
        "content-length-keys": in_content_length_keys,
    },
    "environ": {
        "has": env_has,
        "get": env_get,
        "type": env_type,
        "all-http": env_all_http,
        "path-bytes": env_path_bytes,
    },
    "headers": {
        "custom": hd_custom,
        "multiple": hd_multiple,
        "repeated": hd_repeated,
        "set-cookie": hd_set_cookie,
        "www-authenticate": hd_www_authenticate,
        "many": hd_many,
        "latin1-value": hd_latin1_value,
        "mixed-case": hd_mixed_case,
    },
    "method": {
        "echo": me_echo,
    },
    "file-wrapper": {
        "basic": fw_basic,
        "with-content-length": fw_with_content_length,
        "partial": fw_partial,
        "range": fw_range,
        "iterable-fallback": fw_iterable_fallback,
        "small-blocks": fw_small_blocks,
        "wrapped-in-list": fw_wrapped_in_list,
        "closes-file": fw_closes_file,
    },
    "errors": {
        "write": er_write,
        "writelines": er_writelines,
        "stdout": er_stdout,
        "stderr": er_stderr,
    },
    "threads": {
        "identity": th_identity,
    },
}
