Fixed a worker whose default mount is WSGI running on a single thread when it also had an ASGI or gRPC mount: it now has its `threads`.
