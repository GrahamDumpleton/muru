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

Muru is early in its life. What works today is development mode:
`muructl run` serves a WSGI or ASGI application on your machine with
HTTP/1.1, HTTP/2, HTTPS and WebSockets, forked workers, lifecycle hooks
and ASGI lifespan, watch reload, an access log and a small dashboard, and
it has been tested with plain WSGI and ASGI, Flask, Django, Starlette and
FastAPI. The daemon deploys releases from source directories with
blue/green switches, rollback and drains. Traffic management and the rest
of the daemon are still to come.
The [tutorial](tutorials/first-wsgi-app.md) shows what exists, the
[design specification](project/design/index.md) describes what is being built
and the [roadmap](project/roadmap.md) describes in what order. Pre-releases
of the 0.1.0 line are on PyPI: `uv tool install muructl` for the command and
`muru` in your application's environment for the runtime library.
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
