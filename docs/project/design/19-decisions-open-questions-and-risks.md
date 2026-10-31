# 19. Decisions, open questions and risks

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

## 19.1 Decisions taken

| Decision | Choice |
| --- | --- |
| Name and binaries | Project muru. One binary muructl with daemon and edge subcommands. Runtime library distribution and import name muru |
| Process layout | Separate edge processes from the daemon in production, embedded in one process in development. Both built as libraries |
| Edge to worker protocol | Own framing with protobuf payloads over Unix sockets, one request per pooled connection (ADR 0021, superseding ADR 0001). gRPC remains for the control plane and admin API |
| Worker link server | Rust inside the extension, started after fork. No grpcio anywhere in the worker |
| Application threads | A fixed pool of long-lived threads with persistent Python thread states, each request in a fresh contextvars context (ADR 0022) |
| WSGI and ASGI in one worker | Requests handed across per request in either direction: the muru.wsgi_to_asgi environ key and the asgi_to_wsgi scope extension, with lifespan registration and a WebSocket adapter under muru.asgi (ADR 0023) |
| Preload | auto by default everywhere: preload with an import check, falling back to per-worker imports when the import left sockets, threads, child processes or event loops behind (sections 6.1 and 6.2) |
| Releases | Immutable. Configuration changes are new releases |
| Provisioning | One procedure, muructl build, called by muructl run, the daemon's Building state and the image build stage. Install from locks, never resolve. One knob, build.when: auto builds what is missing and leaves what exists alone, always builds on every start and lock change, never builds nothing; the same key in development and under the daemon. A missing interpreter is installed only where an environment would be built (ADR 0036) |
| Environment builds | Development builds the project's own .venv with the developer's uv, as uv sync would; the daemon builds into STATE/envs/APP/RELEASE as the release's user with --no-dev and no source builds. Building needs pyproject.toml, uv.lock and uv; a machine without uv runs on an existing environment and is told how to get uv (section 8.2) |
| uv | External program, optional muructl[uv] extra. Development uses the developer's uv as it is; the daemon isolates its directories but keeps the project's own [tool.uv] configuration, passing UV_CONFIG_FILE rather than UV_NO_CONFIG for builds (ADR 0036) |
| Garbage collection | Measured in every worker through gc.callbacks, on by default with no setting, since the callback costs far less than the shortest collection; the generation label is what the interpreter reports, full on free-threaded builds. Muru does not yet decide when collection runs (ADR 0038) |
| Runtime library | Provided by muructl on the host's PYTHONPATH in every mode, one directory per interpreter tag, fetched as a wheel and unpacked without pip or uv; never a required application dependency. A matching copy already in the environment is used as it is and nothing is fetched, so offline machines need no flag (ADR 0037) |
| Python versions | CPython 3.11 and later, free-threaded from 3.14t |
| Platforms | Linux for production, macOS for development, no Windows |
| wsgi.input_terminated | Always True. chunked_body defaults to buffer |
| wsgi.multiprocess | True for daemon-managed releases, False for single-process development runs |
| Body size limit | 10 MB by default, as in mod_wsgi-express. Routes that accept larger bodies raise it explicitly |
| Containers | Host and workers in the container, daemon and edge outside; muructl is PID 1 with a built-in init and the host is a subreaper under it, so no tini (ADR 0035) |
| Error override | Off by default |
| Reload | Off by default. Every reload is a new release |
| Licence | Apache-2.0 |
| Order of interfaces | Prove the architecture with WSGI first (phases 0 and 1). ASGI, including lifespan and WebSockets, follows in phase 2 |
| Repository and registry | github.com/GrahamDumpleton/muru, with container images on ghcr.io/grahamdumpleton |
| Dashboard | Server-rendered askama templates with htmx, Server-Sent Events and uPlot, via the JSON gateway. Built with cargo only. Minimal read-only version in muructl run from phase 1 |
| Managed HTTP server releases | In scope for migration cutovers in phase 3: fixed instances, server profiles, full traffic management, no Python-specific features (section 9.9) |
| Free-threading | Supported from phase 1 with the WSGI bridge, with the extension designed for it from the outset (section 6.11) |
| Runtime wheels | Version-specific wheels for every supported CPython, regular and free-threaded. No abi3 |
| gRPC mounts and authentication sub-requests | In initial scope, phase 3 (sections 6.13 and 4.16) |
| TLS cryptography | rustls, tokio-rustls and rcgen on the ring provider in every crate; aws-lc-rs banned by cargo-deny so cmake never becomes a prerequisite (ADR 0024) |
| Retries | A request is sent again only when no worker has read it: a worker that cannot be connected to or closes its connection before the head is written is skipped for another. Nothing is sent again after a worker has read the head, whatever the method (ADR 0032) |
| Worker liveness | Native, in every worker: a thread attaches to the interpreter every 500 ms and the worker reports to its host every second; either quiet for gil_stall_timeout (30 s) replaces the worker as the hard timeout does. HTTP liveness probes stay opt-in for application failures (ADR 0031) |
| Worker group routing | A group's `match` takes `client_class`, `host` and `method` lists, never headers, which are a client class; groups rank by how many lists they give, then by prefix length. An optional group's requests wait as for a group at zero workers and never fall through to the default group. A group's `mounts` subset is served by its workers but imported by the host, with the default mount always served (ADR 0033) |

## 19.2 Open questions

1. Resolved by ADR 0021: the phase 0 benchmarks put gRPC 24 to 37 percent behind mod_wsgi daemon mode on hello world and the framed link ahead of it, so the framed link replaced gRPC on the edge-to-worker link (section 5.1).
2. Retention defaults for warm superseded releases on memory-constrained nodes. Provisional choice, to review later: 30 minute retention, scaled down to the host plus one worker per group, dropping to host only and then stopping under memory pressure (section 9.1). After a deploy, the previous release is kept running with no traffic for a retention window (30 minutes by default, section 9.1) so a rollback is instant. That costs the old release's full memory for the whole window. The question is whether the default should stay at 30 minutes, become shorter, or be decided automatically from free memory, for example keeping the old release warm only while the node is under its memory budget and stopping it otherwise (rollback would then need a cold start).

## 19.3 Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Link overhead for small requests | Throughput below alternatives | Retired: the framed link measured at the level of mod_wsgi embedded mode and gunicorn per request (ADR 0021). Coalescing stays |
| GIL contention in the bridge | Lower throughput per worker | Environ built in Rust, GIL released while blocking, batched event delivery, early measurement |
| Runtime wheel matrix | Build and test burden | One build path for all versions in the release workflow, CI matrix limits, new Python versions added as soon as their release candidates appear |
| Preload incompatibilities | Applications break after fork | Import check with automatic fallback to per-worker imports, post_fork hooks, explicit preload settings, clear diagnostics |
| Sticky sessions keeping old releases alive | Slow drains | sticky_grace, drain deadlines |
| Shared state between overlapping releases | Data errors during deploys | Documentation, migration guidance, muructl exec |
| Soft timeout limits | Exceptions not delivered or swallowed | Opt-in, BaseException subclass, hard timeout backstop |
| Event loop starvation in mixed mounts | WebSocket latency | Loop lag signal, per-mount pools, moving mounts to groups |
| Autoscaler instability | Churn or under-provisioning | Sustain windows, asymmetric cooldowns, churn budgets, advisory mode, schedules |
| Root daemon | High-value target | Small privileged surface, no client parsing, capabilities dropped |
| HTTP/3 and ACME maturity | Delayed features | Deferred to phase 4, TLS termination upstream as a workaround |
| Platform differences | macOS behaviour differs | Linux-only production, documented degradations |
| Scope growth | Delays to 1.0 | Phases with exit criteria, future ideas kept out of scope until decided |
