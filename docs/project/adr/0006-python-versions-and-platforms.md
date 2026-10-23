---
status: accepted
date: 2026-10-23
---

# ADR 0006: CPython 3.11 and later, Linux for production, macOS for development

## Context and problem statement

The runtime library ships compiled wheels per interpreter, and the daemon
relies on kernel features for isolation. Which interpreters and platforms are
supported sets the size of the build matrix and the test environments
(specification sections 8.1 and 14.3).

## Decision drivers

- Python 3.10 reaches end of life in October 2026.

- Free-threaded builds (3.14t and later) are a design goal, not an
  afterthought.

- cgroups, subreaper, O_TMPFILE, USS and PSS, inotify and systemd are Linux
  features the design depends on.

- Developers use macOS laptops.

## Considered options

- Support every CPython with security releases, plus PyPy.

- CPython 3.11 to the newest release, free-threaded from 3.14t, no PyPy
  initially; Linux x86_64 and aarch64 (glibc 2.28 or later, and musl) for
  production; Linux and macOS for development; no Windows.

## Decision outcome

Chosen option: the second. macOS is a development platform with documented
degradations (no cgroups, no subreaper, no container releases, one edge
process per pool, named spool files, FSEvents) and muructl check reports
them.

### Consequences

- Good: a bounded wheel and CI matrix; Linux-only mechanisms can be used
  without fallbacks in production.

- Bad: macOS behaviour differs in ways that must be documented and tested
  (case-insensitive file systems, socket path length, fork safety); Windows
  users are excluded.

## More information

Specification sections 8.1, 14.3, 15.6 and 16.2.
