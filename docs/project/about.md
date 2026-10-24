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

- Graham Dumpleton, based in Sydney, Australia.

- Author and long-time maintainer of mod_wsgi, the Apache module for hosting
  Python WSGI applications, first released in 2007 and still widely used,
  including mod_wsgi-express.

- Author of wrapt, the widely used Python library for decorators, wrappers
  and monkey patching, which underpins many observability and
  instrumentation tools.

- Decades of experience with Python web hosting, deployment and operations.

## Why Muru exists

mod_wsgi has lasted a long time. It has also always had its limitations and
its warts, and most of them were never really mine to fix. It runs inside
Apache httpd, which has its own process model and its own ways of handling
concurrency, and those could not be overridden from a module. Daemon mode
got around some of it, mod_wsgi-express papered over more, but the shape of
the thing was always set by the server it lived in.

I have wanted for years to build a better system for hosting WSGI
applications, one designed around Python processes rather than fitted into
something else's. Each time I looked at it, it was too big a mountain to
climb for one person with a day job: an HTTP edge, a control plane, a
process manager and a runtime library, all of which have to be right.

AI coding tools have changed that. The hard slog of writing the code has
largely gone, which leaves me free to spend my time on what I actually
wanted to do: the design and architecture of the system, and the features
I always felt a hosting system for Python needed and could never give it.
Much of that design comes straight from what maintaining mod_wsgi taught
me: daemon process groups, request timeouts, preloading before the fork,
partitioning an application across worker groups, and the trade-offs
between processes and threads. Muru keeps those ideas and drops the
constraints that came with the host they lived in.
