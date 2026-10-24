# Conformance suites

WSGI (PEP 3333, including wsgiref.validate), ASGI 3.0 (HTTP, WebSocket and
lifespan), HTTP/1.1 parsing and request smuggling corpus, HTTP/2 with h2spec,
WebSockets with the Autobahn test suite, and gRPC with the interoperability
test client. See section 15.1 of the design specification.

Run with `just conformance`. External tools are fetched by
`just bootstrap-conformance`, which puts h2spec at `target/tools/h2spec`
(a release binary on linux/amd64 and darwin/amd64, built from source with
Go elsewhere); `wsgi/test_h2spec.py` runs it when it is there and skips
otherwise. Three of its cases are allowed to fail: after an invalid preface
or a client GOAWAY, hyper may close the socket with input unread, which the
peer sees as a reset rather than a clean close.

`wsgi/` is the PEP 3333 suite, plus the HTTP/2 module driven with curl: one server started from
`tests/fixtures/apps/wsgi_conformance.py` for the whole run, and one test
module per area (start_response, response iteration, wsgi.input, environ,
headers, methods, file_wrapper, wsgi.errors, threads). The cases follow the
mod_wsgi integration tests for the same areas, which cover what the
specification leaves to the server as well as what it requires. Shared
helpers for starting muructl and reading responses are in `tests/support`.
