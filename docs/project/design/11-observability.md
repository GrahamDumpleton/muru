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

## 11.7 Debugging

- Status RPC served by Rust threads without the GIL: load, memory and signals.
- Stack dumps of all Python threads on demand (muructl debug stacks), with a faulthandler fallback when the GIL cannot be taken.
- Hard timeouts and memory recycles record stacks and the request that was running.
- Development mode shows tracebacks in error pages and the terminal, and supports --single for debuggers.
