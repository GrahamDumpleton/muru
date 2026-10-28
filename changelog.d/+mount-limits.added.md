Added `concurrency` and `max_connections` on an ASGI mount: the requests
it runs at once on each worker, as slots of its own at the edge, and the
open WebSockets it holds apart from them, so sockets on a `/ws` mount no
longer take the WSGI application's threads.
