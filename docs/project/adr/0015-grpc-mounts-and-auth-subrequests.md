---
status: accepted
date: 2026-10-23
---

# ADR 0015: gRPC service mounts and authentication sub-requests are in initial scope

## Context and problem statement

Two capabilities were first listed as future ideas: serving application gRPC
services from Muru workers, and letting an application authorise requests to
targets that cannot decide for themselves, such as static directories,
upstreams and WebSocket upgrades (specification sections 4.16 and 6.13).

## Decision drivers

- Both reuse mechanisms Muru already has: the typed worker protocol with
  trailers, the thread pool and event loop, internal routes and the trusted
  header model.

- Authentication sub-requests close a gap internal redirects cannot cover
  (WebSocket upgrades after a WSGI decision).

- Existing grpcio servicer code should run unchanged, without grpcio's C core
  creating servers in forked workers.

## Considered options

- Leave both as future ideas.

- Bring both into phase 3 with explicit scope limits (no grpclib or
  betterproto servicers initially; only 2xx auth decisions cached, off by
  default).

## Decision outcome

Chosen option: both in phase 3. gRPC mounts provide the standard health
service, optional reflection and gRPC-Web translation at the edge.
Authentication sub-requests always fail closed and should run in their own
worker group.

### Consequences

- Good: gRPC and HTTP share listeners, virtual hosts, balancing and policies;
  one application can gatekeep for others.

- Bad: more surface in phase 3; gRPC interoperability tests for all four call
  types through h2, h2c and gRPC-Web become an exit criterion.

## More information

Specification sections 4.16, 6.13, 14.5 and 20.2.
