# Roadmap

Muru was built in two stages. A first stage of development built the
core of the design, from the worker protocol to traffic management, in
four phases. What comes next is organised as a few major goals, each
delivered in steps that are useful on their own, and a set of smaller
features added along the way as they fit.

## The foundation: phases 0 to 3

The first stage built what the rest of Muru stands on. Each phase ended
with a usable result and a release.

| Phase | Result | Released as |
| --- | --- | --- |
| 0. Spike | Worker protocol drafts, the PyO3 extension started after fork, a minimal WSGI bridge and a benchmark harness. Chose the framed worker link ([ADR 0021](adr/0021-framed-worker-link.md)) | |
| 1. Single application server | `muructl run` with HTTP/1.1 and HTTP/2, preload, hooks, process models, a complete WSGI bridge with free-threaded support, watch reload and a minimal dashboard | 0.1.0a2 |
| 2. Daemon and live operations | ASGI, the daemon and state store, admin API and CLI, releases, blue/green, drain and rollback, virtual hosts and TLS, worker groups and mounts, metrics, logs and events, worker inspection, configuration apply and export, systemd integration | 0.1.0a3, 0.1.0a4 |
| 3. Traffic and scaling | Canaries with gates, bucketing and stickiness, the autoscaler, schedules, soft timeouts, internal redirects, uploads, HTTP upstreams, managed HTTP server releases, overlays, authentication sub-requests, gRPC mounts, client classes and rate limits, node budgets and scale to zero, worker profiling and debugger attach | 0.1.0a5, 0.1.0a6 |

The releases since have filled in and corrected what those phases built,
and added interpreter provisioning and environment builds with uv, the
runtime library provided by `muructl` itself, `muructl check`, and a
built-in init with draining on `docker stop` for a container's main
process, handing a request between WSGI and ASGI in code, timestamps on
every log line and the request id on Python log records, and `--json`,
an MCP server and a project skill for coding agents. The
[changelog](changelog.md) lists what each release added, and what is
still to come is in the goals below and in GitHub issues. The scope and exit criteria
of each phase are in [section 14.5](design/14-implementation-plan.md) of
the design specification.

## Major goals

Each goal is delivered in steps. A step leaves something people can use,
so a goal can pause after any step while other work goes ahead.

### Containers

Muru already runs in a container today: the daemon can be a container's
main process, with a built-in init that collects every exited process
and a shutdown that drains in-flight requests on `docker stop`. This
goal goes further, to integration with what container platforms expect
of an application server: health probes, rollouts that lose no
requests, sizing from the container's resource limits, logs that log
collectors keep whole, locked-down security settings, and adding Muru
to any image with one line. That is more than Python web servers
usually provide, and it is what makes running in containers robust
enough for production on Docker, Compose, Podman and Kubernetes.

1. **`muructl serve` in Docker and Compose.** One application in one
   container, configured by flags, environment variables and a mounted
   file: the controller, edge, host and workers all inside, nothing
   persistent, a health check that needs no curl, and a clean drain on
   `docker stop`. Switching an existing Dockerfile to Muru is a one-line
   change.
   Done in 0.1.0a15.
   Issues: [#102](https://github.com/muruhq/muru/issues/102),
   [#103](https://github.com/muruhq/muru/issues/103),
   [#106](https://github.com/muruhq/muru/issues/106),
   [#110](https://github.com/muruhq/muru/issues/110).

2. **Ready for Kubernetes.** Liveness, readiness and startup probes on an
   admin port, a shutdown sequence that loses no requests during a
   rollout, worker counts and memory budgets sized from the pod's
   limits, logs that container log collectors keep whole, metrics for
   autoscaling, and a guide to a Deployment. Done in 0.1.0a15.
   Issues: [#104](https://github.com/muruhq/muru/issues/104),
   [#105](https://github.com/muruhq/muru/issues/105),
   [#107](https://github.com/muruhq/muru/issues/107),
   [#108](https://github.com/muruhq/muru/issues/108),
   [#119](https://github.com/muruhq/muru/issues/119).

3. **muructl in any image.** A Dockerfile adds Muru with one line,
   `uv tool install muructl` in the build stage, on Debian or Alpine
   bases and with every supported Python, and the build fetches the
   runtime library for the image's own interpreter. The container guide
   shows the build with `muructl build` and with uv alone.
   Issues: [#313](https://github.com/muruhq/muru/issues/313).

4. **Image tooling.** `muructl check` recording what an image provides,
   a generated Dockerfile, the image contract and `muructl image check`,
   static files prepared at build time, and `muructl image build`.
   Issues: [#238](https://github.com/muruhq/muru/issues/238),
   [#115](https://github.com/muruhq/muru/issues/115),
   [#130](https://github.com/muruhq/muru/issues/130),
   [#116](https://github.com/muruhq/muru/issues/116),
   [#131](https://github.com/muruhq/muru/issues/131).

5. **Container releases on a node.** The daemon runs a release's host
   and workers in a container from an image, while the daemon and edge
   stay on the node and keep their traffic management.
   Issue: [#128](https://github.com/muruhq/muru/issues/128).

Later additions to the goal: live reload of mounted configuration and
certificates, debugging in a locked-down pod, `muructl exec` for
migrations and one-off commands, and a generated Kubernetes manifest.

### Production readiness and 1.0

What a 1.0 release needs, beside the features it includes:

- **Security controls and review:** the admin group, the daemon dropping
  capabilities it no longer needs, per-release resource limits through
  cgroups, the audit log, then a review of the threat model against what
  is built.

- **Performance:** benchmarks in CI that flag regressions, and tuning
  against the scenarios of the testing strategy.

- **Compatibility:** versioned configuration files, so configuration can
  change after 1.0 without breaking applied files.

- **Release artefacts:** standalone `muructl` binaries with checksums and
  an SBOM, and signed images.

Which of the container steps and smaller features 1.0 includes is
decided along the way; 1.0 follows a production readiness review.
Issues: [#90](https://github.com/muruhq/muru/issues/90),
[#139](https://github.com/muruhq/muru/issues/139),
[#140](https://github.com/muruhq/muru/issues/140),
[#141](https://github.com/muruhq/muru/issues/141),
[#143](https://github.com/muruhq/muru/issues/143),
[#144](https://github.com/muruhq/muru/issues/144),
[#161](https://github.com/muruhq/muru/issues/161),
[#162](https://github.com/muruhq/muru/issues/162),
[#163](https://github.com/muruhq/muru/issues/163),
[#311](https://github.com/muruhq/muru/issues/311).

### Beyond one node

After 1.0: the same admin API managing several nodes, with per-node
daemons, a central controller, edges across nodes, and TCP with mTLS
between components.
Issues: [#146](https://github.com/muruhq/muru/issues/146),
[#147](https://github.com/muruhq/muru/issues/147),
[#148](https://github.com/muruhq/muru/issues/148),
[#149](https://github.com/muruhq/muru/issues/149).

## Smaller features

Features that stand on their own and round out what Muru does. They are
taken up between the steps of the major goals, in no fixed order.

- **Traffic and configuration:** overlays triggered by conditions over
  Muru's own signals
  ([#123](https://github.com/muruhq/muru/issues/123)),
  configuration changes exported to a backup directory
  ([#94](https://github.com/muruhq/muru/issues/94)), per-group
  recycling limits and `max_surge`
  ([#150](https://github.com/muruhq/muru/issues/150),
  [#151](https://github.com/muruhq/muru/issues/151)), and rollouts
  whose capacity follows traffic, with a guide to upgrading in place on
  a host with limited memory
  ([#321](https://github.com/muruhq/muru/issues/321)).

- **Edge and HTTP:** ACME certificates
  ([#133](https://github.com/muruhq/muru/issues/133)), HTTP/3
  ([#134](https://github.com/muruhq/muru/issues/134)), several
  edge processes and edge pools
  ([#132](https://github.com/muruhq/muru/issues/132)),
  connection handling under pressure and per listener
  ([#154](https://github.com/muruhq/muru/issues/154),
  [#155](https://github.com/muruhq/muru/issues/155),
  [#156](https://github.com/muruhq/muru/issues/156)),
  compression per host and route and request decompression
  ([#152](https://github.com/muruhq/muru/issues/152),
  [#153](https://github.com/muruhq/muru/issues/153)), wildcard
  and regular expression host names and host-level settings
  ([#170](https://github.com/muruhq/muru/issues/170),
  [#171](https://github.com/muruhq/muru/issues/171)), static
  file listings and a metadata cache
  ([#172](https://github.com/muruhq/muru/issues/172),
  [#174](https://github.com/muruhq/muru/issues/174)), Unix
  socket listeners
  ([#157](https://github.com/muruhq/muru/issues/157)), proxy
  settings per listener and client certificate headers
  ([#316](https://github.com/muruhq/muru/issues/316),
  [#317](https://github.com/muruhq/muru/issues/317)), 100 Continue
  over HTTP/2 ([#319](https://github.com/muruhq/muru/issues/319)).

- **Logging, events and OpenTelemetry:** daemon-wide logging defaults
  ([#169](https://github.com/muruhq/muru/issues/169)), log files
  reopened on rename and a choice of what happens when output stalls
  ([#166](https://github.com/muruhq/muru/issues/166),
  [#167](https://github.com/muruhq/muru/issues/167)), event sinks
  to files and through the admin API
  ([#164](https://github.com/muruhq/muru/issues/164),
  [#165](https://github.com/muruhq/muru/issues/165)), OTLP
  metrics, log records and request tracing
  ([#136](https://github.com/muruhq/muru/issues/136),
  [#137](https://github.com/muruhq/muru/issues/137),
  [#135](https://github.com/muruhq/muru/issues/135)).

- **systemd:** journald as the log and event sink
  ([#159](https://github.com/muruhq/muru/issues/159)), socket
  activation for the daemon's listeners
  ([#160](https://github.com/muruhq/muru/issues/160)).

- **Applications and servers:** a uWSGI profile for managed servers
  ([#95](https://github.com/muruhq/muru/issues/95)).

## Ideas

Ideas recorded for later evaluation, from
[section 20](design/20-future-ideas.md) of the specification and since,
each an issue so that nothing foreseen is lost: among them traffic
capture and replay, shadow traffic, predictive scaling, resumable
uploads, sub-interpreters, the PROXY protocol, an admin client for
applications, ASGI response trailers, and builds without uv.
They are not committed work until taken up.

## How work is tracked

Every piece of planned work is a
[GitHub issue](https://github.com/muruhq/muru/issues) in the
repository: bugs, the steps of each major goal, the smaller features and
the ideas.

- The [project board](https://github.com/users/GrahamDumpleton/projects/1)
  shows every open issue of Muru and of its
  [workshops](https://github.com/muruhq/muru-workshops) with its
  status (backlog, next, in progress, in review, done), priority, size
  and goal (one of the major goals, a smaller feature, housekeeping or an
  idea), and is the place to see what is being worked on and what comes
  next.

- Each step of a major goal is a tracking issue whose sub-issues are the
  work of that step, so its progress shows on the issue.

- Labels say what kind of work an issue is: `bug`, `enhancement`,
  `documentation` and `ci`, `containers` for running Muru in a container
  or a Kubernetes pod, `decision` for an issue where a choice has to be
  made before work starts, and `tracking` for the issue of a goal's step.

- A [milestone](https://github.com/muruhq/muru/milestones) for
  each upcoming release collects the issues planned for it.

- The specification remains the intended design. An issue says what is
  to be built and names the section that describes it, and a change that
  departs from the specification updates it and adds a decision record.

Bug reports and feature requests come in through the issue forms, as
[contributing](contributing.md) explains.
