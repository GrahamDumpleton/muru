# Muru

Muru is a routing edge, control plane and Python runtime for serving WSGI and
ASGI applications, with zero-downtime releases, traffic management and
Python-aware process management.

A Rust edge accepts client connections and routes requests over a private link to
Python worker processes, which a Rust controller manages as immutable
releases that can run side by side. A new release, possibly on a different
Python version, starts beside the old one, and traffic moves when it is
ready: blue/green, weighted canaries, A/B splits, previews and instant
rollback are policy changes, not restarts.

```{admonition} Status
:class: warning

Muru is in alpha: the 0.1.0 pre-releases are on PyPI, and commands and
configuration may still change between them. Development mode,
`muructl run`, serves a WSGI or ASGI application on your machine. The
daemon serves applications on one node: releases with blue/green
switches, canaries with gates, traffic policies, previews, rollback and
drains, virtual hosts with TLS, worker groups that scale, overlays and
schedules, uploads, mounts, upstreams, client classes, metrics, logs,
events and a dashboard, declared in files or on the command line. Both
are tested with plain WSGI and ASGI, Flask, Django, Starlette and
FastAPI. Still to come are uv provisioning of interpreters, container
releases, ACME certificates, HTTP/3, OpenTelemetry export, several edge
processes and more than one node. The
[tutorial](tutorials/first-wsgi-app.md) is the place to start, the
[how-to guides](howto/index.md) cover each feature and the
[roadmap](project/roadmap.md) says what comes next. Run
`uv tool install muructl` for the command, and add `muru` to your
application's environment for the runtime library.
```

```{admonition} A note on how Muru is built
:class: note

Muru is developed with extensive use of AI coding agents, directed and
reviewed by its creator, under a written design, decision records,
comprehensive tests and continuous integration, with review of everything
that is merged. People who prefer not to use or depend on AI-assisted
software may find the project is not for them. See [About](project/about.md).
```

```{toctree}
:maxdepth: 2

tutorials/index
howto/index
reference/index
explanation/index
project/index
```
