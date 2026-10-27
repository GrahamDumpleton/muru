# Muru

[![CI](https://github.com/GrahamDumpleton/muru/actions/workflows/ci.yml/badge.svg?branch=develop)](https://github.com/GrahamDumpleton/muru/actions/workflows/ci.yml)
[![Documentation](https://img.shields.io/readthedocs/muru?label=docs)](https://muru.readthedocs.io/)
[![PyPI muructl](https://img.shields.io/pypi/v/muructl?label=muructl)](https://pypi.org/project/muructl/)
[![PyPI muru](https://img.shields.io/pypi/v/muru?label=muru)](https://pypi.org/project/muru/)
[![Python versions](https://img.shields.io/pypi/pyversions/muru)](https://pypi.org/project/muru/)
[![Licence](https://img.shields.io/github/license/GrahamDumpleton/muru)](https://github.com/GrahamDumpleton/muru/blob/main/LICENSE)

Muru is a routing edge, control plane and Python runtime for serving WSGI and
ASGI applications, with zero-downtime releases, traffic management and
Python-aware process management.

A Rust edge accepts client connections and routes requests over a private link to
Python worker processes, which a Rust controller manages as immutable
releases that can run side by side. A new release, possibly on a different
Python version, starts beside the old one, and traffic moves when it is
ready. Blue/green, weighted canaries, A/B splits, previews and instant
rollback are policy changes, not restarts. The same model runs from
`muructl run` on a laptop to a large monolith under systemd.

## Status

Muru is in alpha: the 0.1.0 pre-releases are on PyPI, and commands and
configuration may still change between them.

- **Development mode:** `muructl run` serves a WSGI or ASGI application on
  your machine with HTTP/1.1, HTTP/2, HTTPS and WebSockets, forked workers
  with an import check before the fork, lifecycle hooks and ASGI lifespan,
  watch reload, an access log and a dashboard.

- **The daemon** serves applications on one node. It deploys releases
  from source directories and moves traffic between them by blue/green
  switch, by canary with gates, or by traffic policy with previews and
  sticky clients, with rollback and drains. It serves virtual hosts with
  TLS, scales worker groups, applies overlays on schedules, and has
  uploads, mounts mixing WSGI, ASGI and gRPC, internal redirects,
  authentication sub-requests, HTTP upstreams, client classes and rate
  limits, metrics, logs, events, a dashboard and systemd integration,
  declared in files or on the command line.

Both are tested with plain WSGI and ASGI, Flask, Django, Starlette and
FastAPI on regular and free-threaded Python. Still to come are uv
provisioning of interpreters, container releases, ACME certificates,
HTTP/3, OpenTelemetry export, several edge processes and more than one
node; the [roadmap](docs/project/roadmap.md) lists the phases. The
[tutorial](https://muru.readthedocs.io/en/latest/tutorials/first-wsgi-app.html)
is the place to start, and the
[documentation](https://muru.readthedocs.io/) has a guide for each
feature. The [design specification](docs/project/design/index.md) and
[architecture decision records](docs/project/adr/index.md) describe how
Muru is built.

```
uv tool install muructl          # the command
uv add muru                      # the runtime library, in your application's environment
muructl run app:application      # http://127.0.0.1:8080
```

## What it does

- Serve WSGI and ASGI applications with no separate web server in front:
  HTTP/1.1, HTTP/2, TLS, virtual hosts, static files, uploads and internal
  redirects are handled by the edge.

- Deploy without downtime: immutable releases, draining, blue/green,
  canaries with gates, deterministic bucketing, sticky sessions, previews and
  rollback.

- Manage Python processes: preload before fork with an import check and
  hooks, worker groups per path, free-threaded builds, memory policies, soft
  and hard timeouts, and Python-aware scaling.

- Mix WSGI and ASGI in one process: mounts by path, each served by the
  side it was written for. Still to come, a WSGI view will be able to hand
  a request to an ASGI application on the event loop, which is how a WSGI
  application will take WebSockets, and an ASGI application to hand one to
  a WSGI callable on the thread pool.

- Reconfigure live through an API and CLI, with declarative files that
  export and apply round-trip, plus metrics, structured logs, an event
  stream and a built-in dashboard. OpenTelemetry export is still to come.

## Install

Install the command line once with `uv tool install muructl` and add the
`muru` runtime library to each application's environment with `uv add
muru`. Both are pre-releases of the 0.1.0 line, so `pip install --pre` is
needed where pip is used instead of uv. Pre-built wheels cover Linux and
macOS on CPython 3.11 to 3.15, including the free-threaded builds.

## Five-minute start

In development:

```
muructl run app:application --reload     # HTTP on 8080, restarts on change
muructl run --https                      # HTTPS on 8443 with a local certificate
muructl run . --port auto                # the project's [tool.muru] table, free ports
```

Under the daemon:

```
muructl up && muructl deploy shop .      # a daemon with zero-downtime deploys
muructl apply -f config/                 # applications, virtual hosts and TLS from files
muructl deploy shop . --hold             # a new release, started without traffic
muructl rollout canary shop@r2 --wait    # traffic in steps, judged by gates, rolled back on failure
muructl dashboard login                  # a sign-in link, when the daemon serves a dashboard
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
