# About

## How Muru is built

Muru is developed with extensive use of AI coding agents, directed and
reviewed by its creator. The process is deliberate: a written
[design specification](design/index.md), [decision records](adr/index.md)
for every significant choice, comprehensive tests and continuous integration,
and review of everything that is merged. The rules the agents work under are
published in the repository's `AGENTS.md`, and they apply to people too.

This is stated plainly, and without apology. People who prefer not to use or
depend on AI-assisted software may find the project is not for them.

## The creator

```{admonition} Draft
:class: note

This outline is to be confirmed and completed by Graham.
```

- Graham Dumpleton, based in Sydney, Australia.

- Author and long-time maintainer of mod_wsgi, the Apache module for hosting
  Python WSGI applications, first released in 2007 and still widely used,
  including mod_wsgi-express. Much of Muru's design (daemon process groups,
  request timeouts, preloading, vertical partitioning, the trade-offs of
  process and thread models) comes directly from lessons learned maintaining
  it.

- Author of wrapt, the widely used Python library for decorators, wrappers
  and monkey patching, which underpins many observability and
  instrumentation tools.

- Decades of experience with Python web hosting, deployment and operations.

## Why Muru exists

To be written, in Graham's words: what the existing options get wrong, what
Muru keeps from mod_wsgi, and how AI is used to build it.
