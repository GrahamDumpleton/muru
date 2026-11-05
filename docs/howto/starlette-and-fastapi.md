# Starlette and FastAPI

Nothing in either framework needs to know about Muru. Point `muructl run`
at the ASGI application the framework already gives you; it is detected
as ASGI and served on the worker's event loop.

Both frameworks, and Django's ASGI handler, are part of Muru's own test
suite. The examples use development mode; under the [daemon](daemon.md)
the same entrypoint is deployed.

## Starlette

```
muructl run app:application
```

where `app.py` holds `application = Starlette(routes=[...])`. Routes,
middleware, `WebSocketRoute` endpoints and `FileResponse` work as they do
under any ASGI server. A `lifespan` context manager runs once in each
worker, and whatever it yields is the `request.state` of every request in
that worker.

## FastAPI

```
muructl run main:app
```

Path operations, dependency injection, `UploadFile` and `Form`
parameters (with `python-multipart` installed), `WebSocket` endpoints and
the `lifespan` context manager all run unchanged. A path operation
defined with `def` rather than `async def` runs on a thread, as FastAPI
arranges, so it cannot stall the loop.

## Django's ASGI handler

```
muructl run mysite.asgi:application
```

Django's `get_asgi_application()` serves HTTP only and has no lifespan
support, which Muru allows: the worker starts without one. Synchronous
views run on threads through Django's own `sync_to_async` handling.

## Settings that matter

- `--concurrency` (default 1000) is how many requests one worker holds on
  its loop at once; `--processes` adds workers.

- `--sync-threads` (default 8) is how many `def` endpoints and
  dependencies, and `asyncio.to_thread` calls, a worker runs at once:
  Starlette and FastAPI run synchronous code on anyio's worker threads,
  whose limit Muru sets to it. See
  [where synchronous code runs](asgi.md#one-event-loop-per-worker).

- The body size limit, timeouts and HTTPS options of `muructl run` apply
  exactly as they do to WSGI applications.
