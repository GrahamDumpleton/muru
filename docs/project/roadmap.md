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
mounts and the groups `mounts` subset later). Next from phase 2: the
metrics and logs.
