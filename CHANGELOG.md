# Changelog

All notable changes to Muru are recorded here. The file is assembled from
fragments in `changelog.d/` by towncrier when a release is prepared; do not
edit the released sections by hand.

Muru follows semantic versioning. Before 1.0, minor versions may contain
breaking changes, which are listed under each release.

<!-- towncrier release notes start -->

## 0.1.0a4 (2026-10-25)

### Added

- Added `muructl daemon upgrade`, which re-executes the daemon from the
  installed binary without interrupting traffic: the daemon keeps its pid,
  takes its hosts and edge over again, and replaces the edge with one from
  the new version, the old edge finishing its connections first.
- Added `muructl debug status`, `stacks`, `tasks` and `inspect`, which ask a
  release's workers what they are doing through the daemon and the host,
  and `muructl logging level`, which changes logger levels in the workers
  for a period.
- Added built-in log rotation: the daemon rotates the access log, the
  edge's and each release's log and its own daily or at a size, compresses
  the archives and keeps the newest, from the `[logs]` table; `rotate =
  'external'` leaves rotation to logrotate.
- Added charts over time to the dashboards (requests, errors and latency
  over the last hour or day), served as JSON on `/v1/history` and streamed
  with Server-Sent Events, and the daemon's events stream on
  `/v1/events/stream`, which the page uses to show events as they happen.
- Added connection limits: `max_connections` per listener with a fast 503
  when full, `max_connections_per_client`, keep-alive timeouts (longer for
  trusted proxies), `max_requests_per_connection` and the HTTP/2 stream
  limit, in the daemon's `[limits]` table or as `muructl run` flags, with
  the open and refused connections on `/metrics`.
- Added memory limits for workers: `[tool.muru.memory]` or `--memory
  KEY=VALUE` set a soft limit, over which a worker reclaims and is then
  recycled, and a hard limit, over which it is replaced at once, measured
  as USS, PSS or RSS; each worker's memory and requests are served on
  `/metrics`.
- Added per-application users: `[app.NAME]` takes `user`, `group` and
  `umask`, which a daemon running as root applies to the application's
  hosts and workers, and a daemon that is not root reports as not applied.
- Added response compression at the edge: responses the client accepts are
  compressed with br, zstd or gzip, for the configured content types and
  sizes, from the daemon's `[compression]` table or by default under
  `muructl run` (off with `--no-compression`).
- Added secrets: stores of the file, env and exec kinds in the daemon's
  configuration, secret references in a release's env table or with
  `deploy --secret`, delivered as variables or files when the release
  starts and never recorded or shown, and `muructl secret` for file stores.
- Added static routes, which serve a directory from the edge with index
  files, conditional requests, ranges, precompressed variants, cache
  headers and a fallback to an application or a file, and `muructl
  serve-static DIR` for serving a directory in development.
- Added worker recycling and the hard timeout: `max_requests` and
  `max_lifetime` replace a worker with a fresh one on a new socket with no
  request failed, and a request past `--timeout hard=DURATION` has its
  worker taken out of routing, its stacks written to the log and the worker
  replaced, with `worker.recycled` and `worker.timeout_killed` events.

### Changed

- Changed a daemon that runs as root to start the edge, and every
  application that names no `user`, as a service user (`muru`, or the one
  `--user` or the `[daemon]` table names) and never as root; it refuses to
  start when the account does not exist, and `user = 'root'` on an
  application is recorded as an `app.user_is_root` warning each time a
  release starts.

### Fixed

- Fixed `muructl run` and `muructl serve-static` being killed by a Ctrl-C or
  SIGTERM that arrived while the workers were still starting; the run now stops
  in order, the workers running their drain and exit hooks, whenever the signal
  comes.
- Fixed a relative `--python` for `muructl deploy`, or a relative `python`
  in a file for `muructl apply`, being resolved where the daemon starts
  the host rather than where the command ran or the file is; a bare
  command name is still looked up on the PATH.
- Fixed the dashboard's history and events streams delivering only their
  first message after the page loaded, so the charts and the events table
  stood still until a reload.


## 0.1.0a3 (2026-10-25)

### Added

- Event webhooks: `[[events.sink]]` entries in the daemon's configuration file deliver events as signed CloudEvents requests, filtered by type patterns and levels, in order and at least once from a cursor that survives receiver outages and daemon restarts.
- Logs under the daemon: `muructl logs APP[@RELEASE]` shows or follows a release's output and `muructl logs --access` the access log, whose records now name the application, release, host, group, mount and snapshot; `muructl logs reopen [--workers]` (or SIGUSR1 to the daemon) reopens every log file after logrotate has renamed them.
- Mounts: `[mounts.NAME]` tables in `[tool.muru]` or under a declared release serve other entrypoints at path prefixes from the same workers, any mix of WSGI and ASGI, with the mount's path as `SCRIPT_NAME` or `root_path`.
- Prometheus metrics: the daemon serves request counters, timing histograms, in-flight, queue depth and slot gauges and the workers ready at `/metrics` on the address `--metrics` or the `[metrics]` table names, `muructl metrics` prints the same text, and a development run serves it from the dashboard.
- Python logging defaults: a `[tool.muru.logging.python]` table (or a declared release's `logging.python`) sets the root logger's level, the format of loggers without handlers (`standard`, `json` or a format string), warning capture and named loggers' levels before the application is imported, published as `MURU_LOG_LEVEL` and `MURU_LOG_FORMAT`, with `muru.logging.default_config()` giving the same defaults as a `dictConfig` dictionary.
- Running under systemd: the shipped unit starts the daemon as Type=notify and the daemon reports readiness with sd_notify; SIGTERM detaches it as `muructl daemon stop` does, `muructl daemon reload` (or SIGHUP) re-reads the `[proxy]` table, and `muructl daemon stop --timeout` bounds the wait for the daemon to exit.
- The daemon serves a read-only dashboard and JSON gateway on a loopback address given with `--dashboard` or a `[dashboard]` table: the node, every application's releases with their workers and slots, hosts and routes, recent requests, events and each serving release's log tail, with `/metrics` beside them.
- The daemon: `muructl daemon start` (or `muructl up`) runs a daemon with
  its own state store, runtime and log directories, and `muructl deploy APP
  SOURCE` creates a release from a source directory, starts it, moves
  traffic to it when every worker is ready and drains the release that
  served before, so no request is refused during a deploy; the replaced
  release stays warm for 30 minutes (`--retain`) so a rollback within that
  window is instant. `muructl release
  list` and `wait`, `rollback`, `drain`, `events` and `status` manage and
  watch it; a release whose host dies is started again. The edge runs as a
  process of its own on a socket the daemon binds and hands to it, and is
  started again on the same socket if it dies. `daemon stop` detaches,
  leaving the edge and the hosts serving for the next daemon to take over,
  so a daemon restart costs no request; `daemon stop --shutdown` drains
  and stops everything.
- Trusted proxies: a `[proxy]` table in the daemon's configuration file names the peers whose Forwarded, X-Forwarded-For, -Proto, -Host, -Port, -Prefix and X-Real-IP headers are believed; forwarding headers from anyone else are removed, and the resolved client, scheme, host, port and prefix reach the application, with the proxy in `muru.proxy_addr`.
- Virtual hosts and TLS in the daemon: `[host.NAME]` and `[[route]]` tables applied with `muructl apply`, listeners with `tls = true` serving each host's certificate by SNI, a default host and 421 for unknown names, path-prefix routes, HTTPS redirects, `muructl host list` and `muructl route list`, and SERVER_NAME from the matched name.
- Worker groups: `[groups.NAME]` tables in `[tool.muru]` or under a declared release give a release named groups with their own process and thread counts, selected by path prefix at the edge, with the group in `muru.context()` and `MURU_GROUP`.
- `muructl apply -f FILE or DIR` deploys the applications and releases a declarative file declares, with `--dry-run`, `--prune` and `--json`; `muructl config export` writes desired state as such a file and `muructl config diff -f` compares one with it. Release tables take a `source` key, which `muructl run --print-config` now writes.
- `muructl run` now serves ASGI applications: an entrypoint whose callable is
  a coroutine function, or one declared with `--interface asgi`, runs on one
  event loop per worker with the lifespan protocol (startup before the worker
  is ready, shutdown on stop, and the lifespan state copied into every
  request's scope), request bodies as `http.request` messages,
  `http.disconnect` when the client goes away or the response completes, a
  sync thread pool for `asyncio.to_thread` and `sync_to_async`
  (`--sync-threads`, default 8) and `--concurrency` (default 1000) requests
  in flight per worker. WebSockets, trailers and the file extensions follow.
- `muructl run` serves WebSockets to ASGI applications: the edge completes
  the handshake once the application accepts (with its subprotocol and
  headers), relays whole text and binary messages in both directions, carries
  close codes each way, closes a socket with 1009 for a message over 1 MiB,
  pings idle clients and closes a silent socket after five minutes. Closing
  before accepting answers 403, and the `websocket.http.response` extension
  lets the application decline with a full response. HTTP/2 extended CONNECT
  is accepted as well.

### Fixed

- An application that fails after starting its response but before any of it
  has reached the edge now gets the 500 error page, with the traceback in
  development mode, instead of a dropped connection.
- The edge binds its HTTPS listener together with the HTTP one, before it connects to the workers, so a client that finds one port open during start-up finds both.
- `muructl run` now stops in order on SIGTERM as it does on Ctrl-C, and a host
  whose parent has gone away still stops its workers; before, both could
  leave worker processes running.

### Documentation

- Starlette, FastAPI and Django's ASGI handler now run through the test
  suite under `muructl run`, including WebSockets and lifespan state, an
  ASGI conformance suite pins the scope, message, lifespan and WebSocket
  rules, the Autobahn test suite checks the WebSocket wire protocol, and a
  how-to page covers the frameworks.


## 0.1.0a2 (2026-10-24)

### Added

- A first pre-release that serves requests. `muructl run` is development
  mode for one WSGI application: install `muructl` with `uv tool install
  muructl` and `muru` in the application's virtualenv, then `muructl run
  app:application`. The daemon, releases, traffic management and ASGI are
  still to come; the roadmap says when.
- Added HTTPS to `muructl run`: `--https` serves TLS on port 8443 (`--https-port`)
  next to HTTP, `--https-only` redirects HTTP to it, `--tls-cert` and
  `--tls-key` serve your own certificate, and otherwise a development
  certificate for localhost, 127.0.0.1, ::1 and any `--host` names is issued
  by your mkcert authority when one is installed or self-signed when not, kept
  in the user cache directory. HTTP/2 is negotiated over TLS, and the
  application sees `wsgi.url_scheme`, `HTTPS` and `muru.tls`.
- Added `--port` to `muructl run`, with `--port auto` picking free ports and
  printing them, and `--bind` now takes an address alone. A port already in
  use is reported with the process holding it, found through lsof or ss.
- Added `--print-config` to `muructl run`, which writes the declarative
  equivalent of the run (listeners, limits and the release) as the TOML that
  `muructl apply` will read, instead of running.
- Added `muructl run`, which serves one WSGI application behind a preloaded, forked worker pool over the framed worker link, on a fixed pool of long-lived application threads (the phase 0 spike).
- Added the WSGI conformance suite under `tests/conformance/wsgi`, run by `just conformance`, with cases taken from the mod_wsgi integration tests, and brought the bridge up to it: start_response validation and latin-1 header encoding, reason phrases, Content-Length enforcement, `wsgi.file_wrapper`, request headers with underscores dropped, and application exceptions that no longer end the worker.
- Each worker's stdout and stderr are pipes the host relays to the terminal, every line tagged with the worker's index (and the stream for stdout), so output from different workers can be told apart.
- Error responses the edge generates are content negotiated (HTML, an RFC 9457 problem document or plain text) and name their status, reason and request id; in development mode an application error's page includes the traceback. Every response carries X-Request-ID, a time-ordered identifier that is also muru.request_id in the environ.
- Flask and Django fixture applications (`tests/fixtures/apps`, installed from the `fixtures` dependency group) run through `muructl run` in the integration suite: forms and uploads, cookies and sessions, redirects, streaming, file responses, framework error pages and application errors.
- Preload modes for the host: auto runs the import check after preloading and falls back to per-worker imports when the import left behind a connected socket, pipe, thread or child process, naming the module responsible; true fails instead and false imports in every worker. Lifecycle hooks (on_load, pre_fork, post_fork, on_ready, on_drain, on_exit and on_reopen_logs on SIGUSR1) come from the module named by `muructl run --hooks`, and `muru.context()` reports the application, worker and process model.
- Request bodies without a Content-Length are read in full at the edge so the application sees CONTENT_LENGTH, bodies over the body size limit are refused with 413, hop-by-hop headers never reach the application, and a client sending Expect: 100-continue is told to continue only once the application starts reading, so a request it rejects first never carries its body across the link.
- The conformance suite sends malformed and ambiguous HTTP/1.1 requests, from conflicting framing headers to oversized heads, and checks that none reaches the application.
- The edge enforces the timeouts of section 7.6 with their defaults: a request waiting more than 5 s for a worker gets 503 queue_full, one without a response head after 300 s gets 504 deadline and its worker connection is dropped, a client that stops sending its body for 60 s gets 408, a worker that stops mid-response for 60 s is cut off, and request heads are limited to 64 KiB and 100 headers with 30 s to arrive. `muructl run --timeout NAME=DURATION` adjusts them.
- The edge serves HTTP/2 over cleartext by prior knowledge on the same listener as HTTP/1.1, with HTTP_HOST taken from the :authority pseudo-header. HTTP/2 over TLS follows with TLS support. `just bootstrap-conformance` fetches h2spec and the conformance suite runs it.
- `muructl run --reload` watches the project for changed Python files (with --reload-dir, --reload-include and --reload-exclude to adjust what), starts a new release from the same source when they change, switches the edge to it once its workers are ready and stops the previous release, which finishes the requests it is serving. A release whose import fails is reported and the previous one keeps serving. The watcher polls for now; inotify and FSEvents come with the daemon.
- `muructl run --single` serves in the host process with one thread and no fork, so a debugger attached to it sees the application. The hooks run there too, except pre_fork, which has no fork to precede.
- `muructl run` now takes a project directory, or nothing, as well as
  `module:callable`, reading the entrypoint and settings from the project's
  `[tool.muru]` table in pyproject.toml or from muru.toml; flags override the
  table. Added `--factory` (the entrypoint builds the application, per worker
  after post_fork, or in the host with `factory_phase = "host"`) and
  `--interface`, and renamed `--workers` to `--processes`. An ASGI entrypoint
  is refused with a message saying ASGI arrives in phase 2, and a worker that
  keeps failing at start stops the run instead of being forked forever.
- `muructl run` serves a read-only dashboard on loopback (http://127.0.0.1:8081/ by default, `--dashboard` to move or switch it off): workers and their in-flight requests, requests per second and latency percentiles over the last minute, recent requests and the host and worker output, refreshed by the embedded htmx.
- `muructl run` writes a compact access log line per request to the terminal, with the status, bytes, timings for queue, application and total, the worker and the request id. `--access-log` switches it off, limits it to errors and requests slower than `--slow`, or sends JSON lines to a file, and `--access-log-exclude` hides matching paths.

### Changed

- Added Python version classifiers to the muru distribution, so PyPI and the README badge show the supported versions, and moved both distributions from the Planning to the Alpha development status.
- `muructl run` now defaults to one process, reports `wsgi.multiprocess` as
  False for a single-process run, accepts request bodies up to 100 MB
  (`--max-body-size` changes it, and `muru.max_body_size` in the environ
  shows the limit in force) and warns once when the application sends a
  Strict-Transport-Security header, which browsers would apply to every
  server on localhost.
- `muructl run` now says at start-up and on the dashboard that it is
  development mode, refuses to bind an address outside loopback, and offers
  `--allow-public` to serve one anyway with the development behaviours off
  (no tracebacks in error pages, the 10 MB body limit, no dashboard unless
  asked for).

### Fixed

- A request's end of output is now signalled explicitly by the WSGI bridge. It was inferred from the drop of the start_response object, which Flask and Django keep alive in an exception's reference cycle after an error response, so the worker connection stayed busy until a later garbage collection and the next request on it hung.
- Fixed the dashboard's requests per second and error count under load: they
  were derived from the in-memory access log, which keeps the last 1000
  requests, so the rate could not rise past what that ring covered. The access
  log now keeps per-second counters for the last minute, which are exact at
  any rate.

### Documentation

- The documentation is also published as a PDF on Read the Docs. Mermaid diagrams appear there as source listings.


## 0.1.0a1 (2026-10-23)

### Added

- First pre-release, published to claim the muru and muructl names on PyPI and to exercise the release pipeline. Nothing runs yet: `muructl` reports that it is not implemented.
