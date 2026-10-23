---
status: accepted
date: 2026-10-23
---

# ADR 0002: Separate edge processes from the daemon in production

## Context and problem statement

The daemon holds desired state, reconciles it and supervises hosts. The edge
carries traffic. They could run in one process or as separate processes
(specification section 2).

## Decision drivers

- Traffic must keep flowing while the daemon restarts or upgrades.

- Only a small privileged core should ever run as root, and it should parse
  no client traffic.

- Development should need one command and one process, with no daemon.

## Considered options

- One process holding controller and edge, in every mode.

- Separate edge processes spawned by the daemon, with listener sockets passed
  over SCM_RIGHTS, in production; the same two components embedded as threads
  of one process in development.

## Decision outcome

Chosen option: separate processes in production, embedded in development.
Both the edge and the controller are built as libraries so the same code runs
in either layout. The daemon creates listening sockets and passes them to
unprivileged edge processes spawned from its own executable image, so edge
and controller always run the same version until an explicit upgrade.

### Consequences

- Good: the daemon is never on the request path; edges can be replaced with a
  listener handoff and no refused connections; edge pools give process-level
  isolation.

- Bad: two process layouts to test, a shared memory table for exact
  least-outstanding balancing across several edge processes, and a handoff
  protocol for edge replacement.

## More information

Specification sections 2.1, 2.3, 4.2 and 13.4.
