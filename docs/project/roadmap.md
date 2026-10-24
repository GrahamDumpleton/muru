# Roadmap

Work proceeds in phases, each with a usable result and explicit exit
criteria. The full table, with scope and exit criteria, is in
[section 14.5](design/14-implementation-plan.md) of the design specification.

| Phase | Result |
| --- | --- |
| 0. Spike | Worker protocol drafts, the PyO3 extension started after fork, a minimal WSGI bridge and a benchmark harness. Decides the edge-to-worker link |
| 1. Single application server | muructl run with HTTP/1.1 and HTTP/2, preload, hooks, process models, a complete WSGI bridge with free-threaded support, watch reload and a minimal dashboard. Replaces gunicorn for one application in development |
| 2. Daemon and live operations | ASGI, the daemon and state store, admin API and CLI, releases, blue/green, drain and rollback, virtual hosts and TLS, worker groups and mounts, metrics, logs and events, worker inspection, configuration apply and export, systemd integration |
| 3. Traffic and scaling | Canaries with gates, bucketing and stickiness, the autoscaler, schedules, soft timeouts, internal redirects, uploads, HTTP upstreams, managed HTTP server releases, overlays, authentication sub-requests, gRPC mounts, client classes and rate limits, worker profiling and debugger attach |
| 4. Hardening and extras | Condition-triggered overlays, uv provisioning, container releases and base images, multiple edge processes, ACME, HTTP/3, scale to zero, OTLP export, security review, performance tuning. 1.0 |
| 5. Multi-node | Per-node daemons, central control, TCP with mTLS between components |

Current position: phase 1 in progress. Phase 0 landed the framed worker
link chosen by [ADR 0021](adr/0021-framed-worker-link.md), a fixed pool of
long-lived application threads and the benchmark harness. Phase 1 so far
has the WSGI conformance suite (`just conformance`) and the bridge
behaviour it pins down, request body handling at the edge (buffering, the
size limit and 100-continue), the host lifecycle (preload modes with the
import check, hooks and `muru.context()`), HTTP/2 with h2spec, negotiated
error pages and request identifiers, the access log, the edge timeouts,
watch reload, tagged worker output, Flask and Django fixtures, the
minimal read-only dashboard in `muructl run`, the HTTP/1.1 parsing corpus
and HTTPS in development with certificates from mkcert or self-signed
([ADR 0024](adr/0024-ring-tls-provider.md) picks the ring provider).
Also in: projects run from their `[tool.muru]` table with factories and
interface detection, `--port auto` and the port-in-use message, the
development defaults of section 13.1, `--print-config`, and the many-threads
stress test of section 6.11, which the VM runs on the free-threaded build.
Sanitizer builds moved to the future ideas (section 20.10). Still to come
from phase 1: the check of the exit criteria in section 14.5 against a real
project.
