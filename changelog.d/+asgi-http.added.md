`muructl run` now serves ASGI applications: an entrypoint whose callable is
a coroutine function, or one declared with `--interface asgi`, runs on one
event loop per worker with the lifespan protocol (startup before the worker
is ready, shutdown on stop, and the lifespan state copied into every
request's scope), request bodies as `http.request` messages,
`http.disconnect` when the client goes away or the response completes, a
sync thread pool for `asyncio.to_thread` and `sync_to_async`
(`--sync-threads`, default 8) and `--concurrency` (default 1000) requests
in flight per worker. WebSockets, trailers and the file extensions follow.
