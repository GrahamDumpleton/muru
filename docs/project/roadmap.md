# Roadmap

Work proceeds in phases, each with a usable result and explicit exit
criteria. The full table, with scope and exit criteria, is in
[section 14.5](design/14-implementation-plan.md) of the design specification.

| Phase | Result |
| --- | --- |
| 0. Spike | Worker protocol drafts, the PyO3 extension started after fork, a minimal WSGI bridge and a benchmark harness. Decides the edge-to-worker link |
| 1. Single application server | muructl run with HTTP/1.1 and HTTP/2, preload, hooks, process models, a complete WSGI bridge with free-threaded support, watch reload and a minimal dashboard. Serves one WSGI application in development |
| 2. Daemon and live operations | ASGI, the daemon and state store, admin API and CLI, releases, blue/green, drain and rollback, virtual hosts and TLS, worker groups and mounts, metrics, logs and events, worker inspection, configuration apply and export, systemd integration |
| 3. Traffic and scaling | Canaries with gates, bucketing and stickiness, the autoscaler, schedules, soft timeouts, internal redirects, uploads, HTTP upstreams, managed HTTP server releases, overlays, authentication sub-requests, gRPC mounts, client classes and rate limits, node budgets and scale to zero, worker profiling and debugger attach |
| 4. Hardening and extras | Condition-triggered overlays, uv provisioning, container releases and base images, multiple edge processes, ACME, HTTP/3, OTLP export, security review, performance tuning. 1.0 |
| 5. Multi-node | Per-node daemons, central control, TCP with mTLS between components |

## Where things stand

Phases 0 to 3 are complete. Phase 0 chose the framed worker link
([ADR 0021](adr/0021-framed-worker-link.md)). Phase 1 was released as
0.1.0a2, phase 2 as 0.1.0a3 and 0.1.0a4, and phase 3 as 0.1.0a5 and
0.1.0a6; the releases since have filled in and corrected what those
phases built. The [changelog](changelog.md) lists what each release
added, and each how-to guide opens with a status note saying what of its
feature is built and what is still to come.

Phase 4 is next. Alongside its own scope, it settles which of the parts
of earlier phases still to come belong in 1.0, and ends with a
production readiness review and the 1.0 release. Phase 5 follows 1.0.

## How work is tracked

Every piece of planned work is a
[GitHub issue](https://github.com/GrahamDumpleton/muru/issues) in the
repository: bugs, the remaining work of each phase, the parts of earlier
phases still to come, and the future ideas of
[section 20](design/20-future-ideas.md) of the specification, so that
nothing foreseen is lost.

- The [project board](https://github.com/users/GrahamDumpleton/projects/1)
  shows every open issue of Muru and of its
  [workshops](https://github.com/GrahamDumpleton/muru-workshops) with its
  status (backlog, next, in progress, in review, done), priority, size
  and the phase it belongs to, and is the place to see what comes next.

- Labels say what kind of work an issue is: `bug`, `enhancement`,
  `documentation` and `ci`, `containers` for running Muru in a container
  or a Kubernetes pod, and `decision` for an issue where a choice has to
  be made before work starts.

- A [milestone](https://github.com/GrahamDumpleton/muru/milestones) for
  each upcoming release collects the issues planned for it.

- The specification remains the intended design. An issue says what is
  to be built and names the section that describes it, and a change that
  departs from the specification updates it and adds a decision record.

Bug reports and feature requests come in through the issue forms, as
[contributing](contributing.md) explains.
