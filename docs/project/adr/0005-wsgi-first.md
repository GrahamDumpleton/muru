---
status: accepted
date: 2026-10-23
---

# ADR 0005: Prove the architecture with WSGI before ASGI

## Context and problem statement

Muru serves both WSGI and ASGI. The order in which the bridges are built
decides what phases 0 and 1 must contain (specification sections 6.5, 6.6
and 14.5).

## Decision drivers

- Validate the worker protocol, fork model and GIL behaviour with the
  simplest bridge first.

- Free-threaded Python support is designed in from the start, and the WSGI
  thread pool is where it matters most.

- ASGI needs an event loop thread, lifespan and WebSockets, which add surface
  without changing the architectural questions.

## Considered options

- ASGI first, WSGI as a thread-pool adapter on top of it.

- Both together in phase 1.

- WSGI in phases 0 and 1, ASGI including lifespan and WebSockets in phase 2.

## Decision outcome

Chosen option: WSGI first, ASGI in phase 2. The WSGI bridge is complete, with
free-threaded support, by the end of phase 1; ASGI follows with its
conformance suite and the Starlette and FastAPI fixtures.

### Consequences

- Good: phase 0 benchmarks and the fork-safety demonstration are smaller;
  phase 1 can replace gunicorn for one application in development.

- Bad: WebSocket support arrives a phase later; the ASGI bridge must fit the
  interpreter-handle keyed internals designed for WSGI.

## More information

Specification sections 6.5, 6.6, 6.11, 6.12 and 14.5.
