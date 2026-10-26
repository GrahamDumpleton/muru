# Roadmap

Work proceeds in phases, each with a usable result and explicit exit
criteria. The full table, with scope and exit criteria, is in
[section 14.5](design/14-implementation-plan.md) of the design specification.

| Phase | Result |
| --- | --- |
| 0. Spike | Worker protocol drafts, the PyO3 extension started after fork, a minimal WSGI bridge and a benchmark harness. Decides the edge-to-worker link |
| 1. Single application server | muructl run with HTTP/1.1 and HTTP/2, preload, hooks, process models, a complete WSGI bridge with free-threaded support, watch reload and a minimal dashboard. Serves one WSGI application in development |
| 2. Daemon and live operations | ASGI, the daemon and state store, admin API and CLI, releases, blue/green, drain and rollback, virtual hosts and TLS, worker groups and mounts, metrics, logs and events, worker inspection, configuration apply and export, systemd integration |
| 3. Traffic and scaling | Canaries with gates, bucketing and stickiness, the autoscaler, schedules, soft timeouts, internal redirects, uploads, HTTP upstreams, managed HTTP server releases, overlays, authentication sub-requests, gRPC mounts, client classes and rate limits, worker profiling and debugger attach |
| 4. Hardening and extras | Condition-triggered overlays, uv provisioning, container releases and base images, multiple edge processes, ACME, HTTP/3, scale to zero, OTLP export, security review, performance tuning. 1.0 |
| 5. Multi-node | Per-node daemons, central control, TCP with mTLS between components |

Current position: phase 3 in progress, its first half released as
0.1.0a5. Phase 0 landed the
framed worker link chosen by [ADR 0021](adr/0021-framed-worker-link.md), a
fixed pool of long-lived application threads and the benchmark harness.
Phase 1, released as 0.1.0a2, is development mode for one WSGI application:
the WSGI conformance suite (`just conformance`) and the bridge behaviour it
pins down, request body handling at the edge, the host lifecycle (preload
modes with the import check, hooks, factories and `muru.context()`),
projects run from their `[tool.muru]` table, HTTP/1.1 with its parsing
corpus and HTTP/2 with h2spec, HTTPS in development with certificates from
mkcert or self-signed ([ADR 0024](adr/0024-ring-tls-provider.md) picks the
ring provider), negotiated error pages and request identifiers, the access
log, the edge timeouts and limits, watch reload, tagged worker output, the
development defaults and the loopback rule of section 13.1,
`--print-config`, the read-only dashboard in `muructl run`, Flask and Django
fixtures, and the many-threads stress test of section 6.11 on the
free-threaded build. Sanitizer builds are a future idea (section 20.10).
Phase 2, released as 0.1.0a3 and completed in 0.1.0a4, has the ASGI
bridge: one event loop per worker, lifespan
with its state, the sync thread pool, concurrency declared to the edge in
HELLO, WebSockets relayed by the edge with the MESSAGE and CLOSE frames of
section 5.6 and checked by the Autobahn suite, the ASGI conformance suite,
and plain ASGI, Starlette, FastAPI and Django ASGI fixtures. The daemon's
first slice is in: the state store (SQLite), the locations of section
13.8, the admin API of appendix B over the admin socket with the deploy,
release, rollback, drain, event and status operations, `muructl daemon
run`, `start`, `stop` and `status`, `up`, `deploy`, `release list` and
`wait`, `rollback`, `drain`, `events` and `status`, the blue/green
strategy, the edge as a process of its own (ADR 0002) on a listener the
daemon binds and hands down (respawned on the same socket if it dies,
and handed back over SCM_RIGHTS to a daemon that restarts), and daemon
detach with re-adoption of the edge and the hosts, so a daemon restart
never interrupts traffic, and warm retention of the superseded release
for an instant rollback (whole, until the host control socket brings the
warm floor of section 9.1). Declarative configuration has its first
slice: `muructl apply -f` with dry runs and prune, `config export` and
`config diff` over the ApplyConfig, ExportConfig and DiffConfig
operations of appendix B, for applications, releases, virtual hosts and
routes (listeners and limits stay in the daemon's own file). Virtual
hosts and TLS are in: several listeners handed to the edge, hosts with
exact and single-label wildcard names and a default host (421
otherwise), path-prefix routes to applications, certificates carried in
the snapshot and chosen by SNI, HTTPS redirects, and SERVER_NAME from
the matched name (regular expression names and host-level settings
later). Trusted proxies follow section 4.7: a `[proxy]` table in the
daemon's file, nothing trusted by default, forwarding headers removed
from untrusted peers, the configured types applied from trusted ones
with recursive chain resolution, and the resolved client, scheme, host,
port and prefix carried to the worker as typed fields with the original
peer as `muru.proxy_addr`. Worker groups (section 7.1) are in: named
groups with their own process and thread counts from `[tool.muru.groups]`
or the release's `groups` tables, forked from one host with the group in
each worker's context and MURU_GROUP, their sockets carried per group in
the snapshot and chosen at the edge by the most specific path, in
development mode and under the daemon (scaling modes, per-group timeouts
and memory policy, match lists and optional groups later). Mounts
(section 6.7) are in: named mounts at path prefixes from
`[tool.muru.mounts]` or the release's `mounts` tables, any mix of WSGI
and ASGI loaded in the same workers from one preload, each ASGI
application with its own lifespan on the worker's one loop, the mount
chosen at the edge by the longest prefix and named in the request head,
with its path as SCRIPT_NAME or root_path (per-mount pools, internal
mounts and the groups `mounts` subset later). Logs (sections 11.2 and
11.3) have their first slice: each release's host writes its tagged
output to a file in the log directory and the edge the access log as
JSON lines naming the application, release, host, group, mount and
snapshot, `muructl logs` shows or follows either over the TailLogs
operation, and `muructl logs reopen` or SIGUSR1 makes every writer
reopen its file after an external rotation, with `--workers` running
on_reopen_logs (built-in rotation, journald and the Python logging
defaults later). Metrics (section 11.1) have their first slice: the edge
counts completed requests by status class and reason, keeps queue,
application and total time histograms, and in-flight, queue depth and
slot gauges per group, and reports its samples to the daemon over the
control socket; the daemon adds the workers ready and serves the
Prometheus text exposition on the `[metrics]` listener or `--metrics` and
through `muructl metrics`, and a development run serves `/metrics` from
its dashboard (worker CPU and memory, scaling, edge and TLS metrics and
OTLP export later). The Python logging defaults (section 11.4) are in:
a `[tool.muru.logging.python]` table or a declared release's
`logging.python` sets the root level, the platform handler's format
(`standard`, `json` or a format string), warning capture and named
loggers' levels, applied by the host before the application is imported
by replacing `logging.lastResort` so an application's own configuration
is untouched, published as `MURU_LOG_LEVEL` and `MURU_LOG_FORMAT`, with
`muru.logging.default_config()` for applications that adopt them (the
request id on records, daemon-wide defaults and runtime level overrides
later). Event webhooks (section 11.5) are in: each `[[events.sink]]` of
the daemon's file receives the events its type patterns and levels let
through as CloudEvents 1.0 JSON with an HMAC-SHA256 signature, one at a
time and in order from a cursor in the state store with doubling retries,
so a receiver outage or a daemon restart loses nothing, with
`events.sink.failing` and `events.sink.recovered` events about the sink
itself (OTLP, journald and file sinks, the audit log and sinks managed
through the API later). systemd integration (section 13.3) is in: the
shipped unit runs `muructl daemon run --system` as Type=notify, the
daemon reports readiness and stopping with sd_notify, SIGTERM and SIGINT
detach as `daemon stop` does, SIGHUP and `muructl daemon reload` re-read
the `[proxy]` table, and `daemon stop --timeout` bounds the wait
(reloading listeners, limits and sinks, journald and socket activation
later). The daemon's dashboard (section 10.7) has its first slice:
`--dashboard` or a `[dashboard]` table serves, on a loopback address, the
node, every application's releases with their workers and slots, hosts
and routes, recent requests and events and each serving release's log
tail, as htmx-refreshed pages and as JSON on the gateway paths, with
`/metrics` beside them (charts over time, Server-Sent Events, worker
inspection and mutating actions later). The phase's exit criterion has
its harness: the sustained-load scenarios of `tests/integration/test_load.py`
keep clients on kept-alive connections through deploys, warm and cold
rollbacks, drains and daemon restarts, with every request answered 200.
The daemon upgrade of section 13.4 is in: `muructl daemon upgrade`
re-executes the daemon from the installed binary, keeping its pid and
its children, the new image re-adopts the hosts and the edge from their
records, then starts an edge of its own version on the same listening
sockets, gives it the snapshot and tells the old edge to stop accepting,
finish its connections and exit, under load with zero failed requests
(each edge's sockets are numbered by generation so a replacement never
collides with the edge it replaces, and a respawned edge is recorded so
the next daemon re-adopts it). Worker replacement (sections 7.6 and 7.7)
has its first slice: each worker reports to its host once a second over
a pipe (requests served, in flight and the age of the oldest), the host
serves a control socket on which it announces its worker sockets as they
change and receives the supervisor's acknowledgement, and the daemon and
`muructl run` re-point the edge from those announcements, so a worker
past `max_requests` or `max_lifetime` (both with jitter) is replaced by a
fresh one on a new socket with no request failed, and a worker whose
request runs past the hard timeout leaves routing at once, has every
thread's stack written to the log by faulthandler, is replaced, and is
stopped after `hard_drain_grace` and killed after `shutdown_timeout`,
all recorded as `worker.*` events. The memory policy of section 7.7 is
in: each worker measures its USS, PSS and RSS (from smaps_rollup on
Linux, the task footprint on macOS) at `check_interval` and reports
them, a worker over the hard limit is replaced at once through the hard
path, one over the soft limit for `soft_for` is asked to reclaim
(gc.collect and malloc_trim on SIGUSR2) and recycled if still over, a
worker index recycled for memory three times in an hour raises
`memory.leak_suspected`, and each worker's last report is served as
`muru_worker_memory_bytes`, `muru_worker_requests_total` and
`muru_worker_requests_in_flight` (max_surge, per-group settings, the
node budget, allocator settings, per-request deltas and the request id
on records later). Worker inspection (section 11.7) has its phase 2
slice: each worker has a control connection to its host, created at
fork, on which a thread of the runtime library answers requests after
attaching to the interpreter, and the daemon's Debug operation reaches
it through the host's control socket; `muructl debug status`, `stacks`
(with the request each thread serves, merged across workers showing the
same stack, and SIGQUIT to faulthandler for a worker that cannot
answer), `tasks` (the event loop's tasks and lag) and `inspect`, and
`muructl logging level` for time-limited logger levels. The reading
from outside the process with PEP 768 is deferred by
[ADR 0025](adr/0025-in-process-inspection-first.md); profiling, tracing,
memory tracing, attach, exec and the automatic captures are phase 3.
Response compression (section 4.17) is in: the edge compresses
responses the client accepts with br, zstd or gzip in the configured
preference order, for the listed content types and sizes, chunk by chunk
for streamed bodies, never for a response already encoded or opted out
with X-Muru-Compress or Cache-Control: no-transform, from the daemon's
`[compression]` table (applied on reload) or `muructl run`'s defaults
(per-host and per-route settings, precompressed static variants and
request decompression later). Static files (section 4.9) have their
first slice: a route that names a directory is served by the edge with
index files, ETag and Last-Modified, conditional requests, single
ranges, precompressed variants next to the file, cache headers with
exceptions, a redirect to the slash form of a directory, and a fallback
to an application or a file, never a hidden file, a path that climbs
out, a symbolic link leaving the directory or a name that differs from
the file's on disk, declared through apply and exported, and `muructl
serve-static DIR` serves a directory with the same code (listings,
release static files and the metadata cache later). Connection limits (section 4.18) have their first slice:
`max_connections` per listener with a fast 503 edge_overloaded within
the overload reserve and a close beyond it, `max_connections_per_client`
by address (IPv6 by /64) applied first with trusted proxies exempt,
`keep_alive_timeout` and `keep_alive_timeout_proxy` as the idle timer
of each connection, `max_requests_per_connection` closing with
Connection: close, `http2.max_concurrent_streams`, and the open and
refused counts as metrics, from the `[limits]` table or run flags
(eviction of idle connections under pressure, shrinking idle timeouts,
per-listener settings, HTTP/2 idle closure and Unix socket listeners
later). Secrets (section 10.8) are in: `[secrets.store.NAME]` tables of
the file, env and exec kinds, references from a release's `[env]` table
or `deploy --secret`, resolved when the release starts and delivered as
variables or as files named by NAME_FILE, never recorded, exported or
printed, and `muructl secret set`, `list` and `remove` for file stores
with the value read from standard input. Built-in log rotation (section
11.3) is in: the daemon rotates the access log, the edge's log, each
release's log and its own when the period changes (daily by default, or
hourly or weekly, in UTC) or a size is reached, has every writer reopen
by name, compresses the archive in the background with zstd or gzip and
keeps the newest `keep` archives, from the `[logs]` table, with
`rotate = 'external'` leaving it to logrotate (reopen_on_rename,
on_overflow and journald later). The dashboard (section 10.7) has its
charts and streams: the daemon records a point a second from the edge's
counters (requests and errors per second, the median and 95th
percentile from the histogram, in flight, workers) for an hour at one
second and a day at one minute, serves them on `/v1/history` and as
Server-Sent Events on `/v1/history/stream`, streams events as rendered
rows and JSON on `/v1/events/stream`, and the pages draw the charts with
uPlot and take live events through the htmx SSE extension, both
vendored; a development run has the same charts (worker inspection
views and mutating actions later). Per-application users (section 10.9)
have their first slice: an application's `user`, `group` and `umask`
are recorded, applied and exported, a root daemon gives the release's
directories, secret files and log to the user and starts the host with
that identity, and a daemon that is not root records an
`app.user_not_applied` warning (cgroup limits and the admin group
later). Nothing but the daemon runs as root (ADR 0026): a root daemon
starts the edge, and every application that names no user, as the
service user, `muru` unless `--user` or the `[daemon]` table names
another, refuses to start without that account, which it never creates,
and records an `app.user_is_root` warning for an application that names
root; each release's socket directory opens to the application's user
and the edge's group alone, and a rotated log is recreated with its
owner. Dropping the daemon's own capabilities to the few it needs is
the next step on that path and is still to come. That completes the
phase 2 scope: the exit runs passed in the virtual machine, deploys and
rollbacks under sustained load and daemon and edge restarts and upgrades
with no failed request, and 0.1.0a4 is the release.

Phase 3 has started with traffic policies (section 9.4), which canaries
and the rest of the phase's traffic management build on: a route's
`traffic` table shares its requests between an application's releases by
percentage, each client hashed to one of 10,000 buckets by an anonymous
id cookie the edge issues, a header, a cookie or the client address, with
a salt per route and the newest release's buckets at the top so a client
stays in a growing share; `sticky` keeps a client on its release through
a cookie, with a grace period during which a release leaving the route
stays reachable by cookie alone before it drains; the `Muru-Preview`
header or `muru_preview` cookie pins a release, including one at 0%; the
daemon keeps every release a policy names running, drains one no policy
names, and leaves a superseded release a policy still names in place;
`muructl route list` shows the policy and a change records
`traffic_policy.changed`. The host now tells the runtime library its
application name and release id, so `muru.context()` reports them.
Rollouts (sections 9.2 and 9.3) are in: `muructl deploy --hold` starts a
release without traffic, reachable by preview; `muructl rollout canary`
gives it a growing share of the application's requests step by step, the
share applied on top of the routes' declared policies, each step judged
every second once the candidate has served enough requests, on its 5xx
rate and 95th percentile against the baseline over the same window, so a
fault both releases share does not roll back; a failing gate rolls back
at once and keeps the candidate warm, the last step passing promotes it,
steps can wait for approval, and `rollout status`, `pause`, `resume`,
`promote` and `abort` follow and control the rollout; `muructl rollout
bluegreen` moves all traffic at once; every change is a `rollout.*`
event, and a daemon restart continues a rollout with its current step
starting afresh. Still to
come in this part of the phase: A/B cohorts by header and cookie
matches, per-route policies for one application, and rollouts for an
application reached as the default with no route. Scheduled freezes came with
capacity schedules, and the worker crash, p99, queue wait and probe URL
gates and `config export --resolve-rollouts` followed.

The autoscaler (sections 7.2 to 7.4) followed: a `scaling` table on a
release or a worker group gives the group a policy, spare mode keeping
idle worker equivalents between `min_spare` and `max_spare` and target
mode tracking slot utilization and the p95 queue wait; the daemon reads
the edge's slot and queue gauges and the group's queue wait histogram
each second, forks workers through a new `scale` operation on the host
(workers enter routing once ready, and a drained worker leaves routing
before it is told to stop) and reaps slowly, one per interval, after the
scale-in window, never within the hold after growth, never below the
floor, never a worker younger than its minimum lifetime, and never past
the churn budget, which suspends scale-in and doubles the window when
spent; every decision is a `scaling.decision` event with its reasons,
`muructl scale status` shows the policy, workers, signals and churn,
`muructl scale --processes` pins a group's count through daemon restarts
and `--unpin` hands it back, and `advisory = true` reports without
acting. The decision engine is a module with an injectable clock, driven
through step, ramp, sawtooth, bursty and budget scenarios in unit tests.
Still to come: the node budget manager across applications, the worker
CPU, GIL wait and loop lag signals with the diagnosis table, and scale
to zero.

Scheduled capacity (section 7.5) is in: `[[app.NAME.schedule]]` entries,
one-off or recurring, raise a worker group's floor and ceiling for their
windows and through their warm-up, in batches, with `scale_in = 'hold'`
and the floor divided by traffic share during canaries; each is an
overlay named `APP.NAME`, overlays set the same keys under `apps.NAME`,
and `muructl scale --min N --until TIME` makes a temporary one. Deploy
freezes (section 9.6) refuse deploys, deploying applies, rollouts and
promotions of a frozen application unless `--override-freeze` is given,
recorded as an event, and hold rollouts under way. Still to come:
checking budgets for schedules, with the node budget manager.

The soft timeout (section 7.6) is in: `soft_timeout` in the `timeouts`
table or `--timeout soft=` starts a watchdog thread in each worker that
raises `muru.RequestTimeout` in the thread of a WSGI request past the
timeout, waking a thread blocked in the bridge for the request body or
for room in the response, and cancels the task of an ASGI request; the
bridge logs the traceback and answers 504 `soft_timeout` unless the
application catches the exception and answers for itself, an injection
the request outruns is withdrawn before its thread serves again, and
streaming responses and WebSockets are exempt from the soft and the hard
timeout once their head goes out. The hard timeout must exceed the soft
one by 5 s, checked on `muructl run`, `deploy` and `apply`. Where a
request was when it passed the soft timeout followed with worker
inspection (section 11.7), then `muru.deadline` in the environ and the
scope, and the loop stall timeout, which replaces an ASGI worker whose
event loop has not run for 10 s.

Internal redirects (section 4.10) are in: an application's response with
`X-Muru-Redirect` (always), `X-Accel-Redirect` or a CGI local redirect
(where the release or mount opts in) is drained and the request routed
afresh as a GET (HEAD stays HEAD) with the client's headers, or with its
method and buffered body when `X-Muru-Redirect-Method: preserve` asks;
`X-Muru-Redirect-Host` picks another host and `X-Muru-Redirect-Header-*`
adds trusted headers; a redirect back into the same application is
pinned to the release that redirected, another application's route takes
it only when `accept_redirects_from` names the redirecting application,
and routes and mounts marked `internal` answer 404 to clients; the
target sees REDIRECT_URL, REDIRECT_QUERY_STRING and REDIRECT_STATUS (in
the ASGI scope's muru extension too) and `muru.is_internal()`; the
redirecting response's Content-Disposition, Cache-Control, Expires,
Set-Cookie, Vary and non-empty Content-Type carry over; a chain stops
after 5 hops with 500 redirect_loop under the client's deadline, with
one access log line naming every hop; and a client's X-Muru-* request
headers are stripped at the edge.

File hints (sections 4.9, 5.7 and 6.5) are in, in both forms. By
descriptor: a `wsgi.file_wrapper` object returned as the iterable with
a regular file behind it, and the ASGI `pathsend` and `zerocopysend`
extensions advertised in the scope, pass a duplicate of the descriptor
on a FILE frame with an offset and length, and the edge sends the range
while the application's file is already closed. By path: an
`X-Muru-Sendfile` header, or the second header a release's, mount's or
upstream's `sendfile_header` names, makes the edge serve a file inside
`sendfile_roots`, checked by canonical path, with static-file
behaviour and the application's status and headers around it; a path
outside is 500 sendfile_refused and logged, a missing one 404
sendfile_missing. Trusted upstreams and managed servers give path hints
too, and `muructl deploy --sendfile-root` and `--sendfile-header`,
`apply` and `config export` know the keys.

Upload offloading (section 4.11) is in: a route with `upload =
'multipart'` or `'raw'` makes the edge receive the body first and store
each file in the spool as an unnamed file (a named one where the file
system has no O_TMPFILE, macOS among them), hashing it as it arrives,
and pass the files to the worker by descriptor on the REQUEST frame. A
multipart form reaches the application with each file field replaced by
`FIELD.name`, `content_type`, `size`, a digest field per algorithm and
`ref`, a raw upload with an empty body, and `muru.uploads.files()` and
`get()` give upload objects whose `open()`, `save()` (a link where it
can, a copy otherwise) and `path` reach the files. `max_body_size` on a
route, `max_file_size`, `max_files`, `hash` and a `spool_quota` per
application and for the node answer 413 `upload_too_large`,
`too_many_files` and 507 `upload_spool_full` at the edge, and `precheck
= true` asks the application with the headers alone before the body is
read. Recipes for Flask, Starlette and Django turn the files into each
framework's own upload objects, and the test fixtures run all three.

Client classes, rate limits, method restrictions and route site modes
(sections 4.14 and 4.15) are in. `[[client_class]]` rules in the
daemon's file classify each request by user agent (substrings, regular
expressions or a list file), address (or a list file), headers and
cookies, first match wins, with a claimed crawler confirmed by reverse
and forward DNS in the background; list files are read again when they
change, and the class reaches the access log, `muru_requests_total` and
the application as `muru.client_class`. `[rate_limit.NAME]` token
buckets keyed by address prefix, class, header or cookie, which hosts
and routes name, reject with 429 and Retry-After (and the RateLimit
fields where asked), delay, or only log, counted in
`muru_rate_limited_total` with the `ratelimit.enforcing` event. A
route's `methods` refuses methods with 503 read_only or 405, and its
`mode` reaches the application as `muru.mode`. Routing on a class
followed: a route's `match` makes it a route for some classes, sharing
its path with the route for every class, a traffic policy's `classes`
gives a class a release or upstream of its own, kept running for it,
and a worker group's `match` takes a class's requests ahead of the other
groups, and classes and limits may be declared in applied files too.
Still to come: the application-level mode (section 9.10), buckets shared
between several edge processes, and TLS fingerprints as an input.

Worker inspection's phase 3 commands (section 11.7) are in, through the
in-process control thread of ADR 0025: `muructl debug profile` samples
every thread running Python for a period and writes collapsed stacks for
a flame graph, merged across the workers; `debug trace` counts calls and
time with sys.monitoring on Python 3.12 and later, refusing a busy
worker unless forced; `debug memory` reports sizes, gc state and the
commonest objects, with tracemalloc over a period; and `debug exec` runs
a snippet when the daemon's `[daemon] debug_exec` allows it, each of
these an event. The automatic captures are in too: a request past the
soft timeout is the event `request.soft_timeout` with where it was,
taken before it was interrupted, a hard-timeout kill names where the
oldest request was, and a memory recycle names the commonest objects.
Debugger attach followed: `breakpoint()` in a worker waits for `muructl
debug attach`, out of the timeouts, and `debug attach` carries a pdb
session line by line through the daemon and the host, at the breakpoint
or on a thread of its own, allowed where exec is, and the debugged
worker is out of routing and pinned while the session lasts, and
`--dap` carries the Debug Adapter Protocol to debugpy for an IDE. Still
to come: `--requests` and `--path`, pstats output, and reading from
outside with PEP 768.

Overlays (section 9.10) are in, activated by hand or by a one-off or
recurring (cron and duration) schedule in a named time zone: declared in applied files, they reach
routes by a new route `name` and set a route's mode, method
restrictions, traffic pin or split, target and rate limits, and an
application's maintenance mode, laid over the base configuration
without changing it, in priority order, with conflicting fields refused
by apply. `preconditions.release_ready` keeps an overlay from moving
traffic to a release that is not ready, a pin holds the application's
rollout, and `muructl overlay list`, `activate` (with `--for` or
`--until`), `end`, `preview` and `promote` manage them, each change an
event; a restarted daemon carries on with the windows it was in.
Advisory schedules only say they would activate, latched ones stay
active until a person ends them, and a warm-up starts a release ahead
of each window, and overlays set scheduled capacity and deploy freezes.
The signed webhook of the `[overlay_webhook]` table activates and ends
overlays with `trigger.external`, `muructl config show --effective`
attributes what active overlays set, and the daemon warns when the clock
schedules follow is not synchronised. An overlay's `error_pages` choose
a route's pages while it is active, and its `client_routes` give client
classes routes of their own, and `config show --effective` also says
which layer each release setting came from. Condition triggers are
phase 4.

Custom error pages and maintenance mode (section 4.12) are in: pages by
status or status:reason at the route, release, host and node levels, as
a file read when the snapshot is compiled, a template the edge renders
with the status, title, reason, description, request id, Retry-After,
host and path, or an application path the edge asks for the page as an
internal sub-request (2 s, never for a capacity failure of the same
application, never recursing, the error's own status and headers kept);
`error_override` with `when` = all, empty_body, not_json or html_only
and the `X-Muru-Error-Override: off` opt-out; `muructl maintenance on`
and `off` over SetMaintenance, serving 503 maintenance at the edge with
exemptions by path, address and bypass cookie, recorded in the store,
announced as `maintenance.changed` and marked in `muructl app list`; and
`muru.error_reason` for handlers. Still to come: the dashboard's
maintenance control.

Authentication sub-requests (section 4.16) are in: a route's `auth`
table names an application path the edge asks, with a bodiless GET
carrying the client's headers, before it serves the route, be it an
application, a static directory or a WebSocket upgrade; 2xx allows the
request and adds the `copy_headers` of the answer to it as trusted
headers, 401, 403 and redirects go back to the client as the handler
answered, and anything else, a failure, an internal redirect or a
timeout is 503 `auth_unavailable`; the handler sees
`muru.auth_request` and the original method, URI and host (as
`X-Original-*` headers too); allowing decisions can be cached for a
`ttl` by listed cookies and headers; `muru_auth_requests_total` counts
decisions by result, and `muructl route list` shows the handler.

HTTP upstreams (section 4.13) are in: an `[upstream.NAME]` table names
an unmanaged HTTP server by `host:port` or `unix:/path` addresses, over
`http1`, `h2c` or `h2` with TLS (`ca`, `server_name`, `verify`), that a
route, a static route's `fallback`, a traffic share named
`upstream:NAME` or an authentication sub-request can target; requests
are re-serialised with forwarding headers and re-framed bodies (ADR
0027 on messages with both Content-Length and Transfer-Encoding),
addresses take requests in turn with a request that was never sent
moving to the next, failed requests eject an address for a growing
period and a `health` table probes a path; `muru_upstream_health`
reports each address, `trust_muru_headers` lets an upstream redirect
internally, WebSockets are relayed to `http1` upstreams, and `muructl
upstream list`, `route list`, `apply` and `config export` know the
tables, and a trusted upstream's `X-Muru-Sendfile` is served within its
`sendfile_roots`.

Managed HTTP server releases (section 9.9) are in: a release with
`kind = 'http'` runs a `command` such as the gunicorn, uvicorn, Granian
or mod_wsgi-express an existing deployment already uses (or any server,
with the `generic` profile and a `stop_signal`), in `cwd`, as one or
more instances each bound to a Unix socket or a loopback port the daemon
allocates and substitutes for `{bind}` or passes through the profile's
own flags; the daemon waits for a readiness probe, starts an instance
again when it exits with a doubling backoff, stops the instances with
the profile's signal and SIGKILL after the grace period, and re-adopts
them after a detach or an upgrade; the edge routes to the release as an
upstream named after it, so traffic policies, canaries, previews, sticky
sessions, drains, warm retention and rollbacks work between it and native
releases of the same application, which is the cutover the phase's exit
criterion asks for; `muructl deploy --server`, the release tables of
`apply` and `config export`, `release list`, the release log and
`muru_upstream_health` know them. Still to come: instance autoscaling
and socket activation (section 20.1).

gRPC mounts (section 6.13) are in: a mount or a release with
`interface = 'grpc'` serves the services its registration function adds
with grpcio-generated code, matched by content type and the method
names its workers declare in HELLO, so gRPC and HTTP share listeners,
hosts and routes; the edge unframes and decompresses request messages,
passes them whole in MESSAGE frames of a `grpc` scope, frames the
responses, carries the status in trailers, bounds each call by its
grpc-timeout (and a unary call by the request deadline), and balances
calls across the workers one by one; synchronous handlers run on
threads and asynchronous ones on the event loop for all four kinds of
call, with grpcio's servicer context; the edge answers the standard
health service, `reflection = true` serves reflection through
grpcio-reflection, and gRPC-Web, binary and text, is translated at the
edge. grpcio has no free-threaded wheels, so gRPC mounts are unavailable
on those builds. Still to come: limiting h2c to trusted networks, CORS
for gRPC-Web, and per-mount thread pools.
