# Roadmap

Work proceeds in phases, each with a usable result and explicit exit
criteria. The full table, with scope and exit criteria, is in
[section 14.5](design/14-implementation-plan.md) of the design specification.

| Phase | Result |
| --- | --- |
| 0. Spike | Worker protocol drafts, the PyO3 extension with tonic started after fork, a minimal WSGI bridge and a benchmark harness. Decides whether gRPC stays on the edge-to-worker link |
| 1. Single application server | muructl run with HTTP/1.1 and HTTP/2, preload, hooks, process models, a complete WSGI bridge with free-threaded support, watch reload and a minimal dashboard. Replaces gunicorn for one application in development |
| 2. Daemon and live operations | ASGI, the daemon and state store, admin API and CLI, releases, blue/green, drain and rollback, virtual hosts and TLS, worker groups and mounts, metrics, logs and events, configuration apply and export, systemd integration |
| 3. Traffic and scaling | Canaries with gates, bucketing and stickiness, the autoscaler, schedules, soft timeouts, internal redirects, uploads, HTTP upstreams, managed HTTP server releases, overlays, authentication sub-requests, gRPC mounts, client classes and rate limits |
| 4. Hardening and extras | Condition-triggered overlays, uv provisioning, container releases and base images, multiple edge processes, ACME, HTTP/3, scale to zero, OTLP export, security review, performance tuning. 1.0 |
| 5. Multi-node | Per-node daemons, central control, TCP with mTLS between components |

Current position: before phase 0. The repository holds the skeleton, the
specification and the initial decision records.
