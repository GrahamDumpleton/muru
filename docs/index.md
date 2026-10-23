# Muru

Muru is a routing edge, control plane and Python runtime for serving WSGI and
ASGI applications, with zero-downtime releases, traffic management and
Python-aware process management.

A Rust edge accepts client connections and routes requests over gRPC to
Python worker processes, which a Rust controller manages as immutable
releases that can run side by side. A new release, possibly on a different
Python version, starts beside the old one, and traffic moves when it is
ready: blue/green, weighted canaries, A/B splits, previews and instant
rollback are policy changes, not restarts.

```{admonition} Status
:class: warning

Muru is at the design stage. Nothing here is released yet. The
[design specification](project/design/index.md) describes what is being built
and the [roadmap](project/roadmap.md) describes in what order.
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
