---
status: accepted
date: 2026-10-23
---

# ADR 0013: Design the extension for free-threaded Python from the outset

## Context and problem statement

Free-threaded CPython builds (3.14t and later) run Python threads in
parallel. An extension written for the GIL and adapted later tends to rely on
the GIL as a lock in ways that are hard to find (specification section 6.11).

## Decision drivers

- Fewer processes with more threads is an attractive model for I/O-bound
  WSGI applications.

- Retrofitting thread safety is more expensive than designing for it.

- The WSGI bridge and its thread pool are built in phase 1 anyway.

## Considered options

- Support free-threaded builds later, after 1.0.

- Support them from phase 1 with the WSGI bridge, with design rules for the
  extension (no GIL as a lock, shared state in Rust, PyO3 detach at every
  blocking point, per-thread state in thread-locals or context variables) and
  runtime checks that report when the GIL has been re-enabled.

## Decision outcome

Chosen option: from phase 1. Conformance and integration tests run on 3.14t
from phase 1 and ThreadSanitizer builds of the extension run nightly.
Running regular and free-threaded builds for one application at once uses
release variants.

### Consequences

- Good: the extension is thread-safe by construction; variants give a
  migration path per path prefix.

- Bad: more CI time; some signals (GIL wait) do not exist on free-threaded
  builds and need replacements; grpcio availability on free-threaded builds is
  still to be confirmed for gRPC mounts.

## More information

Specification sections 6.11, 6.12, 7.1 (release variants) and 16.2.
