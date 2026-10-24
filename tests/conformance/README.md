# Conformance suites

WSGI (PEP 3333, including wsgiref.validate), ASGI 3.0 (HTTP, WebSocket and
lifespan), HTTP/1.1 parsing and request smuggling corpus, HTTP/2 with h2spec,
WebSockets with the Autobahn test suite, and gRPC with the interoperability
test client. See section 15.1 of the design specification.

Run with `just conformance`. External tools are fetched by
`just bootstrap-conformance`.

`wsgi/` is the PEP 3333 suite: one server started from
`tests/fixtures/apps/wsgi_conformance.py` for the whole run, and one test
module per area (start_response, response iteration, wsgi.input, environ,
headers, methods, file_wrapper, wsgi.errors, threads). The cases follow the
mod_wsgi integration tests for the same areas, which cover what the
specification leaves to the server as well as what it requires. Shared
helpers for starting muructl and reading responses are in `tests/support`.
