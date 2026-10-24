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

Current position: phase 2 in progress. Phase 0 landed the
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
Phase 2 so far has the ASGI bridge: one event loop per worker, lifespan
with its state, the sync thread pool, concurrency declared to the edge in
HELLO, WebSockets relayed by the edge with the MESSAGE and CLOSE frames of
section 5.6, and a plain ASGI fixture. Next from phase 2: the Autobahn
suite, the ASGI conformance suite with Starlette and FastAPI fixtures, then
the daemon.
