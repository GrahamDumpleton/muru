# Changelog

All notable changes to Muru are recorded here. The file is assembled from
fragments in `changelog.d/` by towncrier when a release is prepared; do not
edit the released sections by hand.

Muru follows semantic versioning. Before 1.0, minor versions may contain
breaking changes, which are listed under each release.

<!-- towncrier release notes start -->

## 0.1.0a7 (2026-10-27)

### Added

- Added `muructl dashboard login --token`, a bearer token for scripts that read the dashboard's JSON gateway or act through it, and `--url` (or `MURU_DASHBOARD_URL`) to print the login link under the address a tunnel or proxy serves the dashboard at. The dashboards' pages now use relative addresses, so they work behind a proxy that serves them under a path, such as jupyter-server-proxy.

### Changed

- CLI help, messages and warnings no longer refer to the sections of the design specification or to implementation phases, and the `muructl run` warnings that said the daemon was still to come now point to `muructl daemon start`.
- The daemon's dashboard and JSON gateway now need a signed-in session for every page, not only for the actions: `muructl dashboard login` gives a link for a session that may look, `--actions` one that may act, which `actions = true` in the `[dashboard]` table must still allow, and `/metrics` needs none. Scripts that read the gateway need a token from `muructl dashboard login --token`.


## 0.1.0a6 (2026-10-27)

### Added

- Added CORS for gRPC-Web: a release's `grpc_web_origins` has the edge answer preflights and add the CORS headers to calls from the origins it names.
- Added TLS fingerprints: the edge computes the JA4 of each TLS connection's ClientHello, client class rules match it with `ja4` and `ja4_list`, and applications find it in `muru.tls`.
- Added `h2c` on a listener, naming the clients that may speak HTTP/2 in cleartext on it; another client opening with HTTP/2 is answered 505, and its HTTP/1.1 requests are served.
- Added `muru.deadline` to the WSGI environ and `deadline` to every ASGI scope's `extensions["muru"]`: when the edge gives up on the request, in seconds since the epoch.
- Added `muructl config export --resolve-rollouts`, which writes a rollout under way as where it stands: the candidate declared beside the serving release, and the application's routes splitting traffic between them at the candidate's current share.
- Added `muructl debug attach --dap`, which carries the Debug Adapter Protocol between an IDE and debugpy in a worker, through muructl, a waiting `breakpoint()` stopping at its next line once the IDE has attached.
- Added `muructl debug memory --snapshot`, `--diff` and `--drop`, to compare a worker's allocations between two chosen moments, and a traced worker taking no new requests while another worker of its group can.
- Added `muructl debug profile --path` and `--requests` to sample only the requests chosen, and `--format pstats` to write the samples as Python's pstats.
- Added `muructl debug profile`, which samples every thread running Python in a release's workers for a period and writes collapsed stacks for a flame graph, `debug trace`, which counts calls and time with sys.monitoring (Python 3.12 and later), `debug memory`, which reports sizes, gc state and the commonest objects and traces allocations with tracemalloc over a period, and `debug exec`, which runs a snippet in the workers when the daemon's `[daemon] debug_exec` allows it. A request past the soft timeout is now the event `request.soft_timeout` with where it was, and the `worker.timeout_killed` and memory `worker.recycled` events say where the stuck request was and what the worker held most of.
- Added client class routes to overlays: `routes.NAME.client_routes` gives a client class a route of its own at the route's host and path while the overlay is active, changing the route for that class alone or adding a copy of the route, and `muructl overlay preview`, `promote` and `config show --effective` cover them.
- Added client classes and rate limits in applied files: `[[client_class]]` and `[rate_limit.NAME]` tables that `muructl apply` reads are recorded and reach the edge, after the daemon file's, and `muructl config export` writes them out.
- Added client classes and rate limits: `[[client_class]]` rules in the daemon's configuration file classify each request by user agent (substrings, regular expressions or a list file read again when it changes), address, headers and cookies, with claimed crawlers confirmed by DNS, and the class reaches the access log, the `client_class` label of `muru_requests_total` and the application as `muru.client_class`; `[rate_limit.NAME]` token buckets, keyed by address prefix, class, header or cookie and named by hosts and routes, reject with 429 and Retry-After, delay or only log, counted in `muru_rate_limited_total`; and a route's `methods` refuses methods with 503 `read_only` or 405, while its `mode` reaches the application as `muru.mode`.
- Added debugger attach: `breakpoint()` in a worker waits for `muructl debug attach`, out of the request timeouts, and `debug attach` opens a pdb session there, carried line by line through the daemon, or on a thread of its own when no breakpoint waits; it is allowed where the daemon's `[daemon] debug_exec` allows exec.
- Added draining and pinning of a debugged worker: while a `muructl debug attach` session is open the worker takes no new requests and is not recycled, killed or scaled in, and `muructl debug status` shows it as `debugging`.
- Added error page selections to overlays: `routes.NAME.error_pages` chooses the route's page for a status or `status:reason` while the overlay is active, each key in place of the route's own, and `muructl overlay preview` and `config show --effective` show the pages chosen.
- Added gRPC mounts: a mount or a release with `interface = 'grpc'` serves the services a registration function adds with grpcio-generated code, synchronous handlers on threads and asynchronous ones on the event loop, all four kinds of call over h2 and h2c, each call balanced across the workers, with grpc-timeout deadlines, request decompression, the standard health service answered by the edge, server reflection with `reflection = true`, and gRPC-Web translated at the edge. grpcio has no free-threaded wheels, so gRPC mounts need a regular build.
- Added overlays: `[overlay.NAME]` tables in applied files change named routes (site mode, method restrictions, a traffic pin or split, the target and rate limits) and applications' maintenance mode while active, without changing the base configuration, activated by hand with `muructl overlay activate` (for a time or until one) or by a one-off or recurring (cron and duration) schedule in a named time zone, advisory schedules that only report, latched ones that stay active until ended, warm-up that starts a release ahead of each window, with preconditions on a release being ready, conflicts between overlays refused unless priorities settle them, and `muructl overlay list`, `end`, `preview` and `promote`. Routes take a `name` by which overlays reach them. A signed webhook, served from the daemon's `[overlay_webhook]` table, activates and ends overlays declared with `trigger.external = true`; `muructl config show --effective` shows the configuration with the active overlays applied and what each sets; and the daemon warns when the system clock that schedules follow is not synchronised.
- Added reading workers from outside with PEP 768 on Linux and Python 3.14 or later: `muructl debug stacks`, the stacks captured before a hard-timeout kill, and `muructl debug profile` read the worker without its cooperation, so a worker holding the interpreter still shows its stacks.
- Added rollouts for an application reached as the default on a daemon with no host declared: the canary's share and previews apply to its requests as a route's policy would.
- Added rollouts, scaling and maintenance mode to the daemon's dashboard: the rollouts under way and the latest that ended, each worker group's workers, policy, CPU, GIL wait, loop lag, diagnosis and last decision, and when an application went into maintenance, on the page and as `/v1/rollouts` and `/v1/scaling`.
- Added routing on a client class: a route's `match = { client_class = [...] }` makes it a route for those classes, so routes for different classes may share a path; a traffic policy's `classes` gives a class a release, or an upstream, of its own; and a worker group's `match` takes those classes' requests ahead of the other groups. `muructl apply` refuses classes the daemon's file does not define.
- Added scale to zero: a spare or target group with `min_workers = 0` drains its last worker once idle and the next request waits at the edge while one forks, `host_idle_timeout` stops an idle release's host too until the next request starts it, and a retained release keeps one worker per group (`muructl deploy --retain-workers`), dropping to its host alone and then stopping when the node is over its memory budget.
- Added scheduled capacity and deploy freezes: `[[app.NAME.schedule]]` entries, one-off or recurring, raise a worker group's floor and ceiling for their windows, with warm-up, `scale_in = 'hold'` and the floor shared by traffic during canaries; overlays set the same under `apps.NAME`; `muructl scale --min N --until TIME` sets a floor for a while; and `freeze_deploys` refuses deploys, applies that deploy, rollouts and promotions unless `--override-freeze` is given, which is recorded as an event.
- Added shared rate limit buckets: under the daemon the buckets are in shared memory, so a replaced or restarted edge draws from the buckets its predecessor left.
- Added the application-level site mode: an overlay's `apps.NAME.mode`, with `deny` and `answer`, sets the mode and refused methods on every route of the application while the overlay is active.
- Added the dashboard's first action: with `actions = true` in the `[dashboard]` table, a browser signed in through a single-use link from `muructl dashboard login` turns an application's maintenance mode on and off from the page, each action CSRF-protected and recorded as a `dashboard.action` event.
- Added the loop stall timeout: an ASGI worker whose event loop has not run for `loop_stall_timeout` (10 s by default, `--timeout loop_stall=`) is replaced as for the hard timeout, with the event `worker.loop_stalled`.
- Added the node's budget: the `[daemon.budget]` table caps the workers, memory and CPU the releases on the node hold between them, scale-outs are admitted against it with the highest `[app.NAME] priority` first, a release whose memory does not fit is not started once retained releases have made way, schedules are checked against it when applied and ahead of each window, and `muructl daemon status` shows what is held.
- Added the release settings' layers to `muructl config show --effective`: each serving release's entrypoint, interpreter, sizes, recycling limits, timeouts, memory policy and redirect settings, with whether each came from the release table or deploy flags, `[tool.muru]`, the source's `.venv`, the previous release or the default.
- Added the workers' own scaling signals: CPU time, GIL wait and event loop lag, reported every memory check interval, shown by `muructl scale status` with a diagnosis of each group's bottleneck and the tuning it suggests, which every `scaling.decision` event carries too, and `muru_worker_cpu_seconds_total` in the metrics.
- Added threads of a mount's own: a WSGI mount's `threads` is a pool only it uses, or `{ reserved, max }` to reserve some and borrow the shared threads beyond them, and a gRPC mount's `threads` sizes its handlers' pool.
- Added two canary gates: `--max-p99` on the candidate's 99th percentile request time, and `--max-worker-faults` (0 by default) on its worker crashes and timeout kills, which rolls back at once, before the step has the requests the other gates need.
- Added two canary gates: `--max-queue-wait`, the candidate's 95th percentile wait for a worker thread, and `--probe URL`, repeatable, a URL requested through the edge from the candidate and the baseline every 5 seconds by preview, which rolls back when it fails on the candidate alone.
- Added upload offloading: a route with `upload = 'multipart'` or `'raw'` makes the edge receive the body and store each file before the application sees the request, hashing it as it arrives, and pass the files to the worker by descriptor; a multipart form arrives with each file field replaced by `FIELD.name`, `content_type`, `size`, `sha256` and `ref` fields, a raw upload with an empty body, and `muru.uploads.files()` and `get()` give upload objects with `open()`, `save()`, which links the file into place where it can, and `path`. A route's `max_body_size`, `max_file_size`, `max_files`, `hash` and `spool_quota`, and the daemon's `[limits] spool_quota`, are answered at the edge with 413 `upload_too_large` or `too_many_files` and 507 `upload_spool_full`, and `precheck = true` asks the application with the headers alone before the body is read.

### Changed

- A static worker group left above its count when a pin is lifted now returns to its count one worker a minute, instead of keeping the pinned count until the next deploy.

### Fixed

- A release is now `ready` only once the edge has been given it, so a request made as soon as `muructl release wait` returns reaches the new release; before, the wait could return a moment before the edge routed to it, and the request was answered 503.
- Fixed a client class rule whose `user_agent_list` or `address_list` held no entries matching every request: an input from an empty list now matches nothing.
- Fixed a native deploy after a managed server release failing to start: it took the server release's worker count of zero as its own.
- Fixed a worker whose default mount is WSGI running on a single thread when it also had an ASGI or gRPC mount: it now has its `threads`.
- `Upload.save()` on Linux with Python 3.13 and earlier now links an upload without a spool path into place, as it does on 3.14, instead of copying it.
- Fixed the daemon freezing when the routing changed while a worker was stopped or frozen: the edge now gives up on a worker that has not answered HELLO within 5 s and keeps its routing, the change fails naming the worker, and the daemon waits at most 30 s for an edge to apply a snapshot.

### Security

- Updated the zstd library the edge and the daemon use to 0.14.3, for RUSTSEC-2026-0391, RUSTSEC-2026-0398 and RUSTSEC-2026-0403.

### Documentation

- Documented A/B cohorts by header or cookie, a client class given a release by a traffic policy's `classes`, and routes of one application on different releases, in the traffic guide.
- The PROXY protocol is no longer listed as coming: it moved to the specification's future ideas (ADR 0029), and the trusted proxy settings remain the way a proxy passes on the client's address.


## 0.1.0a5 (2026-10-25)

### Added

- Added HTTP upstreams: an `[upstream.NAME]` table names an unmanaged HTTP server, by `host:port` or `unix:/path` addresses and `http1`, `h2c` or `h2` with TLS, that a route, a static route's `fallback`, a traffic share named `upstream:NAME` or an authentication sub-request can target; requests are re-serialised with forwarding headers and re-framed bodies, addresses take requests in turn, one whose requests fail is ejected and tried again gradually, a `health` table probes a path, `muru_upstream_health` reports each address, `trust_muru_headers` lets an upstream redirect internally, and `muructl upstream list` shows what is applied.
- Added authentication sub-requests: a route's `auth` table names an application path the edge asks, with a bodiless GET carrying the client's headers, before it serves the route, whether an application, a static directory or a WebSocket upgrade; 2xx allows the request and adds the answer's `copy_headers` to it as trusted headers, 401, 403 and redirects go back to the client as the handler answered them, and anything else is 503 `auth_unavailable`; the handler sees `muru.auth_request`, `muru.original_method`, `muru.original_uri` and `muru.original_host`, allowing decisions can be cached with `cache = { ttl, key }`, and `muru_auth_requests_total` counts decisions by result.
- Added custom error pages and maintenance mode: `error_pages` by status or `status:reason` on a route, a release (or `[tool.muru]`), a host or in the daemon's file, each a `file`, a `template` with `{status}`, `{title}`, `{reason}`, `{description}`, `{request_id}`, `{retry_after}`, `{host}` and `{path}`, or an application `path` the edge asks for the page as an internal sub-request that sees `REDIRECT_STATUS` and `muru.error_reason`; `error_override` to replace an application's own error responses (`when` = `all`, `empty_body`, `not_json` or `html_only`, opt out per response with `X-Muru-Error-Override: off`); and `muructl maintenance on|off APP`, which serves 503 `maintenance` at the edge with exemptions by path, address and bypass cookie from the release's `[maintenance]` table.
- Added file hints: a `wsgi.file_wrapper` object returned as the response iterable, and the ASGI `http.response.pathsend` and `http.response.zerocopysend` extensions, pass a duplicate of the file's descriptor to the edge on a FILE frame, which sends the range itself while the application's file is closed; an `X-Muru-Sendfile` response header, or the second header `sendfile_header` names such as `X-Sendfile`, makes the edge serve a file inside the release's, mount's or trusted upstream's `sendfile_roots` with static-file behaviour, and `muructl deploy --sendfile-root DIR` and `--sendfile-header NAME` set the keys.
- Added internal redirects: an application answers with `X-Muru-Redirect: /path` (or `X-Accel-Redirect` and the CGI local redirect where a release or mount opts in) and the edge serves the request from there, a protected file from a static route marked `internal = true` or another application path, with the client's headers kept, the application's `Content-Disposition` and cache headers carried over, `REDIRECT_URL`, `REDIRECT_QUERY_STRING` and `REDIRECT_STATUS` for the target, and `muru.is_internal()` to tell such a request. Routes and mounts can be `internal` and name the applications whose redirects may reach them in `accept_redirects_from`; a client's own `X-Muru-*` request headers never reach an application.
- Added managed HTTP server releases: a release with `kind = 'http'` runs a `command` such as gunicorn, uvicorn, Granian or mod_wsgi-express (or any server with the `generic` profile), as instances the daemon binds to Unix sockets or loopback ports, probes for readiness, restarts when they exit and stops with the profile's signal; the edge routes to it as an upstream named after the release, so traffic policies, canaries, drains and rollbacks work between it and native workers of the same application, and `muructl deploy --server`, `apply`, `config export` and `release list` know it.
- Added rollouts: `muructl deploy --hold` starts a release without traffic, `muructl rollout canary` gives it a growing share of the application's requests step by step with gates on its 5xx rate and p95 judged against the release that serves, rolling back on a failing gate and promoting when every step passes, `muructl rollout bluegreen` moves all traffic at once, and `rollout status`, `pause`, `resume`, `promote` and `abort` follow and control a rollout. Each change is a `rollout.*` event.
- Added the autoscaler: a `scaling` table on a release or a worker group with the spare and target modes, bounds, windows and a churn budget sizes the group from the edge's slot and queue signals, every decision an event with its reasons; `muructl scale status` shows the policy, workers, signals and churn, `muructl scale --processes N` pins a group's count and `--unpin` hands it back, and `muru_workers_desired`, `muru_scaling_decisions_total` and `muru_churn_reaps_total` join the metrics.
- Added the soft timeout: `soft_timeout` in a release's `timeouts` table, or `--timeout soft=30s`, interrupts a request that runs past it by raising `muru.RequestTimeout` in its thread (waking one that waits for the request body) or cancelling its ASGI task, logs where it was, and answers 504 `soft_timeout` unless the application catches the exception and answers for itself. The hard timeout must exceed the soft timeout by at least 5 seconds.
- Added traffic policies: a route's `traffic` table shares requests between an application's releases by percentage, with clients hashed to stable buckets by an anonymous id cookie the edge issues, a header, a cookie or their address; `sticky = true` keeps a client on its release through a cookie, with a grace period before a superseded release drains; and the `Muru-Preview` header or `muru_preview` cookie pins a release, including one with no share. Releases a policy names are kept running. `muru.context().release` now reports the release id.

### Fixed

- A client whose WebSocket message was over the limit, or whose frame was malformed, could see the connection reset instead of the 1009 or 1002 close frame: the edge now reads away what the client is still sending before it drops the socket.
- Streaming responses (`text/event-stream` or `X-Accel-Buffering: no`) and accepted WebSockets no longer count toward the hard timeout, which replaced the worker of a long-lived WebSocket as if its request were stuck.
- The daemon's once-a-second work (host and edge checks, retention, log rotation, rollout gates and the autoscaler) no longer stalls while admin requests arrive more often than once a second.
- `muructl daemon stop` and `muructl daemon upgrade` bound every exchange with the daemon, so a daemon that accepts a connection and never answers cannot hold the command past its `--timeout`; the daemon also closes its API connections once the shutdown grace period ends.


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
