# Conformance suites

WSGI (PEP 3333, including wsgiref.validate), ASGI 3.0 (HTTP, WebSocket and
lifespan), HTTP/1.1 parsing and request smuggling corpus, HTTP/2 with h2spec,
WebSockets with the Autobahn test suite, and gRPC with the interoperability
test client. See section 15.1 of the design specification.

Run with `just conformance`. External tools are fetched by
`just bootstrap-conformance`.
