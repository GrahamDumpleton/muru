# Muru

Muru is a routing edge, control plane and Python runtime for serving WSGI and
ASGI applications, with zero-downtime releases, traffic management and
Python-aware process management.

A Rust edge accepts client connections and routes requests over gRPC to
Python worker processes, which a Rust controller manages as immutable
releases that can run side by side. A new release, possibly on a different
Python version, starts beside the old one, and traffic moves when it is
ready. Blue/green, weighted canaries, A/B splits, previews and instant
rollback are policy changes, not restarts. The same model runs from
`muructl run` on a laptop to a large monolith under systemd.

## Status

Muru is at the design stage. Nothing is released and nothing runs yet. This
repository holds the project skeleton, the
[design specification](docs/project/design/index.md) and the initial
[architecture decision records](docs/project/adr/index.md). The
[roadmap](docs/project/roadmap.md) lists the phases, starting with a phase 0
spike that benchmarks the edge-to-worker protocol.

## What it will do

- Serve WSGI and ASGI applications without Apache httpd or nginx, with
  HTTP/1.1, HTTP/2, TLS, virtual hosts, static files, uploads and internal
  redirects handled by the edge.

- Deploy without downtime: immutable releases, draining, blue/green,
  canaries with gates, deterministic bucketing, sticky sessions, previews and
  rollback.

- Manage Python processes the way mod_wsgi daemon mode and gunicorn do, and
  more: preload before fork with hooks, worker groups per path, mixed WSGI
  and ASGI mounts in one process, free-threaded builds, memory policies,
  soft and hard timeouts, and Python-aware scaling.

- Reconfigure live through an API and CLI, with declarative files that
  export and apply round-trip, plus metrics, structured logs, an event
  stream, OpenTelemetry and a built-in dashboard.

## Install

Pre-releases of the 0.1.0 line are on PyPI to claim the names and exercise
the release pipeline; they do not serve requests yet. `uv tool install
muructl` installs the command line, and the daemon will install the `muru`
runtime library for each interpreter it manages. The first version that
works is 0.1.0.

## Five-minute start

Planned:

```
muructl run app:application --reload     # development, HTTP on 8080
muructl up && muructl deploy shop .      # a daemon with zero-downtime deploys
muructl apply -f config/                 # several applications, virtual hosts, TLS
muructl rollout canary shop@r43          # canary with gates and rollback
```

## Contributing

Muru takes contributions as requirements, not as code: open an issue
describing the problem and the desired behaviour, and the maintainer
implements accepted work. See [CONTRIBUTING.md](CONTRIBUTING.md) for the
process and the development setup (`just doctor`, `just bootstrap`).

Muru is developed with extensive use of AI coding agents, directed and
reviewed by its creator, under a written design, decision records, tests and
CI. People who prefer not to use or depend on AI-assisted software may find
the project is not for them. See [About](docs/project/about.md).

## Licence

Apache-2.0. See [LICENSE](LICENSE).
