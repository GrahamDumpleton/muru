# 14. Implementation plan

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

One repository holds a Cargo workspace, the Python package, the dashboard, tests, documentation and packaging. Work proceeds in phases, each with a usable result and explicit exit criteria. This spec is the source of truth; deviations found during implementation are recorded as architecture decision records in docs/project/adr (section 18.5) and folded back into the spec.

## 14.1 Repository layout

```
muru/
  Cargo.toml                 workspace
  rust-toolchain.toml
  proto/
    muru/worker/v1/*.proto
    muru/admin/v1/*.proto
  crates/
    muructl/                 binary: CLI, subcommand dispatch, development mode
    muru-common/             names, ids, errors, durations and sizes, shared types
    muru-config/             schema types, TOML parsing, validation, precedence merge, JSON Schema output
    muru-proto/              frame codec of the worker link, generated prost and tonic code
    muru-edge/               edge library: listeners, TLS, hosts, routing, pipeline, upstreams, static, uploads
    muru-controller/         reconciler, state store, admin API and JSON gateway, rollouts, autoscaler, schedules, events
    muru-supervisor/         spawning hosts, users, cgroups, containers, descriptor passing, log collector
    muru-dashboard/          embedded dashboard assets and handlers
    muru-py/                 PyO3 extension (muru._native): worker link, application thread pool, bridges, control server, watchdog
    muru-testkit/            test harness: spawn daemon, fake workers, HTTP clients, fixtures
  packages/
    muructl/pyproject.toml   maturin, bindings = bin
    muru/pyproject.toml      maturin, PyO3 version-specific extension plus the Python sources
    muru/python/muru/        runtime library Python sources, py.typed and stubs
  dashboard/                 templates, styles and vendored htmx and uPlot
  tests/
    python/                  pytest for the runtime library
    integration/             pytest driving muructl against fixture applications
    conformance/             WSGI, ASGI and HTTP conformance suites
    fixtures/apps/           plain WSGI, plain ASGI, Flask, Django, Starlette, FastAPI, Channels-style WebSocket
  fuzz/                      cargo-fuzz targets
  bench/                     benchmark scenarios and scripts
  docs/                      Sphinx documentation, with the design specification in docs/project/design and decision records in docs/project/adr
  images/                    base image Dockerfiles
  packaging/                 systemd unit, logrotate file, later deb and rpm
  noxfile.py                 Python-side sessions
  justfile                   task runner entry points
  scratch/                   temporary working files, untracked except scratch/.gitignore (which ignores everything else)
```

## 14.2 Key dependencies

| Area | Choice |
| --- | --- |
| Async runtime and HTTP | tokio, hyper 1.x with hyper-util, h2, tower |
| TLS | rustls with tokio-rustls on the ring provider (ADR 0024), rcgen for development certificates, rustls-acme later |
| Worker link | own framing with prost messages (section 5) |
| gRPC (control plane and admin API) | tonic and prost |
| Snapshots and shared state | arc-swap, memfd-backed shared memory for edge load tables |
| CLI and configuration | clap (with generated help and man pages), serde, toml, schemars for JSON Schema |
| State store | rusqlite with bundled SQLite |
| Process control | nix for fork, prctl, sockets and SCM_RIGHTS, sd-notify, cgroups v2 via the file system |
| Containers | Docker Engine API (bollard), which Podman also serves |
| Files | notify for watching, a SHA-256 implementation for uploads |
| Observability | tracing, prometheus-client, opentelemetry with OTLP exporters |
| Python extension | PyO3 with free-threaded support, version-specific builds by maturin |
| HTTP/3 (later) | quinn and h3 |
| Dashboard | Server-rendered askama templates, htmx with its Server-Sent Events extension, and uPlot for charts, vendored and embedded with rust-embed. No Node toolchain |

## 14.3 Platforms and versions

| Item | Supported |
| --- | --- |
| Production | Linux x86_64 and aarch64, glibc 2.28 or later and musl |
| Development | Linux and macOS (arm64 and x86_64) |
| Python | CPython 3.11 to the newest release (3.15 at the time of writing), free-threaded builds from 3.14t |
| Rust | latest stable for releases, minimum supported version at stable minus two, checked in CI |
| Containers | Docker and Podman through the Docker Engine API |

### macOS support and limitations

macOS is supported for development and for trying the operational model, not for production. Almost everything works: muructl run, the daemon, releases, blue/green and canaries, mounts, worker groups, variants, scaling, logging, events, overlays and the dashboard. What differs is kernel-level isolation, resource control and some low-level mechanisms. Production is not supported because those controls are missing and because macOS is not part of the soak, fault-injection and performance testing, so benchmark results there are not representative either.

| Area | Linux | macOS |
| --- | --- | --- |
| Resource limits | cgroups v2 per release, enforced by the kernel | None. Only Muru's memory policy and node budgets apply, and they are advisory |
| Memory measurement | USS and PSS from smaps_rollup | Physical footprint per process. No PSS, so shared preloaded memory cannot be apportioned between workers |
| Orphaned processes | The host is a subreaper and terminates a dead worker's leftover children | No subreaper. Orphans are adopted by launchd, so cleanup relies on process groups, which misses processes that start their own group |
| Container releases | Supported | Not supported. Docker and Podman run containers in a virtual machine, so Unix sockets and descriptor passing do not cross between macOS and containers |
| Several edge processes per pool | SO_REUSEPORT balances connections across processes | SO_REUSEPORT does not balance in the same way, so each pool runs one edge process |
| Upload files | O_TMPFILE, linked into place on save | Named temporary files in a private spool directory, renamed on save, with a sweep for leftovers at start-up after crashes. upload.path uses /dev/fd/N |
| Shared memory | memfd | shm_open |
| File watching | inotify | FSEvents, which coalesces events and adds a little latency |
| Service manager | systemd with sd_notify readiness | launchd plist. Readiness is taken from the admin socket |
| Per-application users | Standard practice | Possible but unusual. Development normally runs everything as the current user |
| Executable for re-exec and edge spawning | /proc/self/exe | Path from the process's own executable lookup, checked for replacement on disk |

Unaffected: tokio uses kqueue instead of epoll, descriptor passing with SCM_RIGHTS works between local processes, sendfile is available, and free-threaded Python builds work.

**macOS behaviours the implementation must handle**

- **Fork safety in system frameworks.** After fork, macOS terminates a child process that uses Objective-C runtime code which may have been initialising in another thread at the time of the fork. Python applications reach this through system APIs, most commonly the proxy lookup used by urllib and libraries built on it. It is a known cause of crashed workers in other pre-forking servers on macOS, and preload makes it more likely. Mitigations: the import check of section 6.2 falls back to per-worker imports when an import started threads, which covers the common cases; muructl run sets no_proxy for workers unless the user has set proxy variables; when a worker dies with this macOS error, the host reports it clearly and points to the documented workaround (OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES), which Muru never sets silently.
- **Unix socket path length.** macOS allows 104 bytes for socket paths and its default temporary directories are long. Run directories default to a short location (for example /tmp/muru-UID), and socket creation fails with a message naming the path and limit rather than a raw system error.
- **Descriptor limits.** The default soft limit is often 256. muructl raises its soft limit to the hard limit at start-up, passes the raised limit to hosts, and warns when the result is too low for the configured connections and workers.
- **Firewall prompts.** Listening on non-loopback addresses can trigger the macOS application firewall prompt. Development mode binds to loopback by default, which avoids it.

The supported platforms reference page in the documentation (section 18.2) carries this table, and muructl check reports the degraded features when run on macOS.

## 14.4 Engineering conventions

- Rust: rustfmt, clippy with warnings denied, no unsafe outside small reviewed modules (fork, descriptor passing, shared memory, PyO3), errors with thiserror in libraries and anyhow at the binary edge, structured logging with tracing.
- Python: ruff for lint and format, mypy in strict mode for the muru package, typed public API with stubs.
- Every user-visible setting is defined once in muru-config, which generates the JSON Schema, the configuration reference documentation and validation.
- Every CLI command is defined once with clap, which generates help, man pages and the CLI reference documentation.
- Definition of done for a feature: implementation, unit tests, an integration test, metrics and events where relevant, documentation, and a changelog fragment.

### Rules for AGENTS.md

The repository's AGENTS.md (with CLAUDE.md pointing to it) carries the following rules, written in the imperative for AI coding agents and applying equally to people. They complement the setup and testing guidance in section 15.6 and the specification and ADR process in section 18.5.

**Writing style, for every file in the repository**

- Do not use em dashes in any file. Rephrase with commas, parentheses, colons or separate sentences.
- In bulleted lists where items run to more than one line, put a blank line between the bullets, in docstrings, Markdown files, code comments and any other prose. This is about the raw file being readable. Be consistent within a list: if one item needs the spacing, space every item in it.

**Python code** (the muru package, tests, fixtures, scripts)

- Always use type hints: on all function and method parameters and return types, and on attributes and variables where the type is not obvious from the assignment. When modifying code that lacks them, add them. mypy runs in strict mode on the muru package (section 14.4).
- Every function, method or property in the public API has a docstring saying what it does, except trivially obvious cases such as an accessor property named for the attribute it returns, and dunder methods implementing standard protocols.
- Put a blank line between a function or method docstring and the first line of code.

**Code layout, for Python and Rust alike**

- Write function bodies in paragraphs: group the statements that together perform one step, and separate the groups with blank lines. Natural boundaries include setup versus main work versus result, before and after a conditional or loop, and around with, try, match or unsafe blocks. Neither cram a body into one block nor put a blank line between every statement.
- Where it helps, start a paragraph with a short comment saying what the step does or why it is needed. Prefer one comment per logical block over line-by-line commentary, and omit it when the code already says it plainly. Put a blank line between such a block comment and the code below it.
- In Rust, every public item in a library crate has a doc comment, and every unsafe block has a SAFETY comment explaining why it is sound.

**Git and review**

- Commit messages never include a co-authored-by trailer or any other agent attribution.
- An agent never commits on its own initiative. It finishes the piece of work, summarises it and waits to be told to commit. Permission to commit covers only the work it was given for and does not carry forward to later steps of a multi-step plan, each of which needs its own review and instruction. Uncommitted changes are how review happens, so committing early makes review harder.
- Work happens on feature branches merged into develop, and releases are made from main through a release pull request from develop (section 17.4). After merging a feature branch into develop and pushing, the work is not treated as landed until the GitHub Actions run for the pushed merge has passed (checked with gh run list --branch develop and gh run watch). Only then is the change reported as on the remote and the feature branch deleted. If CI fails, the feature branch is kept, the failure is reported, and the agent waits for instructions.
- Feature branches are squash-merged into develop, so each change lands as one commit, and release pull requests from develop into main use a normal merge commit, so main keeps the full history of each release. Rebase merges are never used, and pushed branches are never rebased or force-pushed.

**Project process**

- Run just doctor and just bootstrap before starting, and the relevant just test targets before reporting work as complete.
- Read the relevant pages of the design specification before working on a subsystem. When the implementation needs to differ from the specification, propose an ADR and update the specification in the same change instead of deviating silently.
- Add a changelog fragment for user-visible changes, and keep generated references (CLI, configuration, API) up to date by running their generators.
- Use scratch/ for temporary working files, such as reference material given to an agent or plans an agent is asked to write. It exists in the repository but its contents are not part of it: scratch/.gitignore ignores everything except itself. Its contents come and go, so never reference scratch/ files by name from code or documentation that will be committed.

**Enforcement.** The rules that can be checked mechanically are checked in CI and in a pre-commit configuration: a lint step rejects em dash characters (U+2014) in tracked text files, ruff and mypy enforce Python typing and docstring rules for the public API, clippy's missing_docs and undocumented_unsafe_blocks lints cover Rust, and a commit message check rejects co-authored-by and similar attribution trailers. Layout rules such as code paragraphs and bullet spacing are left to review.

## 14.5 Phases

| Phase | Scope | Exit criteria |
| --- | --- | --- |
| 0. Spike | Worker protocol drafts, PyO3 extension started after fork, minimal WSGI bridge, benchmark harness | Hello world, large body and tiny-chunk benchmarks within agreed overhead of gunicorn, uvicorn, Granian and the three mod_wsgi-express configurations, or a decision to replace gRPC with a framed protocol. Fork safety of the extension demonstrated. Outcome: the framed link was adopted (ADR 0021) |
| 1. Single application server | muructl run, edge with HTTP/1.1 and HTTP/2, routing for one application, host, preload, hooks, workers, process models, WSGI bridge complete with free-threaded support, limits and buffering, built-in error pages, watch reload, terminal logs, minimal read-only dashboard in muructl run | WSGI and HTTP conformance suites pass on regular and free-threaded builds. Plain WSGI, Django and Flask fixtures pass their tests through Muru |
| 2. Daemon and live operations | ASGI bridge with HTTP, lifespan and WebSockets (with ASGI conformance, Starlette and FastAPI fixtures), daemon, state store, admin API and JSON gateway, CLI, releases, blue/green, drain, rollback, snapshots, virtual hosts, static TLS, trusted proxies, response compression, connection limits, request identifiers, secrets, per-application users, static files and file hints, worker groups, mounts, hard timeouts, memory policy, metrics, access logs, log collector and rotation, Python logging defaults, events with pull and webhooks, configuration apply, export and diff, systemd integration, full read-only dashboard, worker inspection (status, stacks, tasks, inspect, logging levels, faulthandler) | Deploys and rollbacks under sustained load with zero failed requests. Daemon and edge restarts and upgrades with zero failed requests |
| 3. Traffic and scaling | Canary with gates, bucketing, stickiness, previews, autoscaler (spare and target), churn controls, node budgets, scale to zero (brought forward from phase 4), schedules, soft timeouts, internal redirects and routes, custom error pages and override, maintenance mode, uploads, HTTP upstreams, managed HTTP server releases with profiles for gunicorn, uvicorn, Granian, mod_wsgi-express and generic servers, overlays with schedule, manual and external triggers, authentication sub-requests, gRPC service mounts with health, reflection and gRPC-Web, client classification, rate limits, method restrictions and site modes, worker profiling, tracing and memory tracing, debugger attach and exec, automatic stack capture on timeouts and recycles | Automated canary with rollback on a failing gate. Scaling scenarios meet stability targets without churn. gRPC interoperability tests pass for all four call types through h2, h2c and gRPC-Web. Cutover demonstration: an existing gunicorn deployment runs unchanged as a managed HTTP release, then is canaried to native workers and removed, with zero failed requests |
| 4. Hardening and extras | condition-triggered overlays (advisory first), uv provisioning and builds, container releases and base images, multiple edge processes and pools, ACME, HTTP/3, OTLP export, image tooling, security review, performance tuning | Production readiness review passed. 1.0 release |
| 5. Multi-node | Per-node daemons, central control, TCP with mTLS between components | Same API manages several nodes |

Within each phase, work should follow dependency order: configuration types and protocol first, then the runtime pieces, then control-plane features, then the CLI and documentation for them.
