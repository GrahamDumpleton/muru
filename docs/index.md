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
`muructl run` serves a WSGI application on your machine with HTTP/1.1,
HTTP/2 and HTTPS, forked workers, lifecycle hooks, watch reload, an access
log and a small dashboard, and it has been tested with plain WSGI, Flask and
Django. The daemon, releases, traffic management and ASGI are still to come.
The [tutorial](tutorials/first-wsgi-app.md) shows what exists, the
[design specification](project/design/index.md) describes what is being built
and the [roadmap](project/roadmap.md) describes in what order. The published
pre-releases on PyPI predate all of this; until the next one, Muru is built
from source.
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
