# 11. Observability

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

Muru reports three kinds of data: metrics for continuous measurements, logs for per-request and application output, and events for changes of state. Each has one source of truth and several consumers.

## 11.1 Metrics

Prometheus metrics are served by the daemon on the admin listener (/metrics on the JSON gateway) and, optionally, on a dedicated metrics listener bound to an internal address, never on public listeners by default. Each edge process reports its metrics to the daemon, which aggregates them, so one scrape covers a node. Names use the muru_ prefix and labels app, release, group, mount, route and host where relevant.

| Area | Examples |
| --- | --- |
| Requests | muru_requests_total by status class and reason, muru_request_duration_seconds histograms (queue, application, total), muru_requests_in_flight, muru_retries_total |
| Capacity | muru_queue_depth, muru_queue_wait_seconds, muru_slots_busy, muru_slots_total, muru_async_slots_busy, muru_websocket_connections, muru_handoffs_total by direction, muru_handoff_queue_wait_seconds |
| Workers | muru_worker_cpu_ratio, muru_worker_memory_bytes by kind (uss, pss, rss), muru_gil_wait_seconds, muru_loop_lag_seconds, muru_worker_recycles_total by reason, muru_worker_exits_total |
| Scaling | muru_scaling_decisions_total, muru_workers_desired, muru_workers_ready, muru_churn_reaps_total |
| Edge | muru_edge_connections, muru_tls_handshakes_total, muru_snapshot_version, muru_edge_cpu_ratio per thread, muru_upstream_health |
| Bodies and uploads | muru_body_rejections_total, muru_upload_spool_bytes |

The dashboard reads the same values from in-memory history. OTLP metric export is optional.

## 11.2 Access logs and application logs

- Access logs are structured (JSON by default, or a configurable text format) with request id, host, route, release, group, worker, snapshot version, status and reason, bytes, and timings for queue, application and total, plus internal redirect hops.
- Worker stdout and stderr are per-worker pipes. The host creates them at fork and passes the read ends to the daemon over the control socket. The log collector tags lines with application, release, group, worker and stream, keeps ring buffers for the CLI and dashboard, and writes sinks. Workers are line-buffered. In development mode there is no collector yet, so the host itself relays the pipes to its standard error, each line tagged with the worker's index and, for stdout, the stream (muru.worker[0]: and muru.worker[0].stdout:).
- Sinks: terminal (development), journald (default under systemd), files.

## 11.3 Log rotation

- Writer threads in long-lived processes own every file, so rotation is a handle swap and never involves workers or restarts. Files use O_APPEND, so concurrent edge writers during handoff never interleave within a line.
- Built-in rotation: size and time based, retention count and age, configurable backup names, background compression (gzip or zstd).
- External rotation: logrotate renames files, then runs muructl logs reopen (or sends SIGUSR1). reopen_on_rename detects renames without a signal. copytruncate works but can lose lines. rotate = 'external' disables built-in rotation, and configurations where both would rotate a file are rejected.
- Applications that open their own log files get an on_reopen_logs control message (muructl logs reopen --workers), whose default hook makes logging file handlers reopen on next emit.
- Backpressure never reaches requests: the access log drops and counts records when stalled; application output uses a large drop-oldest queue by default, or blocks if configured.

## 11.4 Python logging defaults

- Before importing the application, the host sets the root logger level and replaces logging.lastResort with a platform handler using the configured format (a format string or json) that writes to stderr. A lastResort handler is used only when no handler exists in a logger's hierarchy, so basicConfig and dictConfig behave exactly as without Muru.
- Optional: request_id and Muru tags on every record, logging.captureWarnings, and a per-logger level map.
- Precedence: Python default, daemon configuration, application, release, the application's own configuration, then time-limited runtime overrides (muructl logging level RELEASE --logger NAME --level DEBUG --for 15m).
- MURU_LOG_LEVEL, MURU_LOG_FORMAT and muru.logging.default_config() let applications adopt the platform defaults explicitly.

## 11.5 Event stream

- One stream of state changes: configuration, releases and rollouts, host and worker lifecycle, recycles and timeout kills, scaling decisions, drains, schedules, edge and daemon upgrades, certificates and warnings.
- CloudEvents 1.0 JSON with a node-wide sequence number (muruseq), subject (app/release/group/worker), actor (user identity or component), correlation ids per rollout or deploy, and a level (info, notice, warning, error). Events never contain secrets or request data. Event types are listed in Appendix C.
- Pull: WatchEvents over gRPC, or Server-Sent Events on the JSON gateway, resumable from a sequence number. muructl events --follow uses the same stream.
- Push sinks with their own filters: webhooks (HMAC-signed, persistent outbox, at-least-once, deduplicate by id), OTLP log records (and spans for rollouts and deploys), journald or syslog, and JSON lines files.
- Retention: 7 days by default, capped by size. The audit log is the long-retained subset initiated through the API.

## 11.6 Tracing

The edge creates a server span per request and forwards W3C traceparent and tracestate to the application in the request head, so OpenTelemetry instrumentation in the application continues the trace. Edge spans and optional worker spans (queue wait, dispatch) are exported over OTLP when configured. Rollouts and deploys can be exported as spans so they appear on the same timeline.

## 11.7 Debugging and live inspection

Every worker has a control connection to its host, and the host to the daemon, so the admin API can reach into any worker of any release: a laptop run, a release in a container, a worker under another user or, in phase 5, on another node. The `muructl debug` commands use that path to find out what a worker is doing and, when needed, to step into it. They share one shape: `muructl debug VERB TARGET [--worker N] [--for DURATION]`, where TARGET is a release or an application, the default is every worker of the release, and the output streams back over the same admin API stream it was requested on, so nothing opens a port.

**Two mechanisms.** Everything below is built on one of these.

- *From outside the process.* Python 3.14 added PEP 768, which lets another process read a running interpreter's frames and run code in it at a safe point; the sampling profiler (`python -m profiling.sampling`), `python -m asyncio ps` and pdb's remote attach are built on it. These need no cooperation from the worker and cost it nothing until used, so they work on a worker that is hung or spinning. The host is the agent: it is the parent, runs as the same user and in a container is PID 1, so it has the rights PEP 768 needs without anything being opened up. On interpreters before 3.14 these commands fall back to the in-process mechanism where one exists and are otherwise unavailable.
- *From inside the process.* A control thread in the runtime library, created by the extension and never holding the GIL while idle, that the host can ask to run named operations: sys.monitoring, tracemalloc, faulthandler, gc, importlib, logging. It stays responsive while the GIL is contended, because it is a Rust-side thread that attaches only to do its work.

**What a worker is doing now.**

- `muructl debug stacks`: the Python stack of every thread, with the request id, path and elapsed time of the request each application thread is serving, from the bridge's own bookkeeping. Taken from outside, so it works on a hung worker, with a faulthandler fallback when the interpreter cannot be read. Across a release the stacks are merged, so sixteen workers waiting on the same lock show as one stack with a count.
- `muructl debug tasks`: for ASGI workers, the asyncio task tree with what each task awaits, from `asyncio ps` and `pstree`, and the event loop lag.
- Automatic capture: when a request passes its soft timeout (section 7.6), before a worker is killed for a hard timeout, and before a worker is recycled for memory (section 7.7), the host takes the stack dump first and attaches it to the event, so "worker killed after 60 s" reads "worker killed after 60 s, waiting in a database connect here".

**Profiling for a period.**

- `muructl debug profile --for 30s`: sampling with the standard library's profiler, of one worker or every worker of a release at once, written locally as collapsed stacks for a flame graph or as pstats. No overhead on the worker and safe on production, which is the point of sampling.
- `muructl debug trace --for 10s`: in-process sys.monitoring for exact call counts and times when sampling is too coarse. It costs the worker something, so it refuses a worker above a load threshold unless forced, and the edge counts the worker as busy while it runs.
- `--requests N --path PREFIX`: profile or trace only the next N requests matching a path or header rather than everything on a busy worker.

**Memory.**

- `muructl debug memory`: resident size and gc generation counts at any time, and object counts by type. With `--trace --for 30s`, tracemalloc for the period and the top allocation sites, or a snapshot diff between two points.
- Before a memory recycle (section 7.7) the host can take a snapshot of the worker it is about to replace, so the leak is attributed rather than just survived.

**Stepping in.**

- `muructl debug attach`: a pdb session in a worker, carried as a byte stream over the admin API. The worker is drained and pinned while the session lasts, its timeouts are suspended, the thread being debugged stays off the pool, and a maximum session length ends a forgotten session. With `--dap` the same tunnel carries the Debug Adapter Protocol for debugpy, so an IDE attaches through muructl rather than through a port.
- `breakpoint()` in application code: the runtime library's breakpoint hook waits for a `muructl debug attach`, raising an event that names the worker, file and line, and gives up after a timeout. Outside Muru the hook does nothing special, so the call is safe to leave in.
- `muructl debug exec -- CODE`: run a snippet in a worker and return its output. Admin role, audited, and off in production unless enabled.

**State and configuration.**

- `muructl debug inspect`: the worker's sys.path, loaded modules with versions and file paths, environment with secrets redacted, resolved settings, open descriptors and sockets, and thread names.
- `muructl logging level` (section 11.4) changes a logger's level for a period and reverts on its own.
- faulthandler is enabled in every worker with its output routed to the log collector, so a crash in a C extension leaves a Python traceback rather than nothing.

**Scope and limits.** Every command is an admin-role operation (section 10.6), is recorded in the audit log, and works through the daemon's existing channels, so nothing listens anywhere new. Reading another process needs the right ptrace permissions: the same user suffices on macOS and on Linux with the default ptrace scope, and in a container the host does the reading from inside. Status, stacks, tasks, inspect, logging levels and faulthandler arrive with the daemon in phase 2; profiling, tracing, memory tracing, attach, exec and the automatic captures follow in phase 3 with the soft timeouts and memory policy they build on (section 14.5).

Development mode shows tracebacks in error pages and the terminal, and supports --single for running the whole application in one process under a debugger (section 13.1). The attach command is for what --single cannot cover: a bug that only shows with forked workers or real traffic, or in the release itself.
