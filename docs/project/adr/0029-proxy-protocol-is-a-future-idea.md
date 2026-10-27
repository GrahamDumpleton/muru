---
status: accepted
date: 2026-10-27
---

# ADR 0029: PROXY protocol is a future idea, not planned scope

## Context and problem statement

Section 4.13 lets a listener accept the PROXY protocol, v1 and v2, from
listed sources, so that a load balancer passing TCP through without
terminating HTTP can tell the edge the client's real address. It was
never built. The question is whether to keep it as planned scope.

## Decision drivers

- Muru deploys Python web applications; it is not meant to be a general
  router, and features earn their place by what Python deployments need.

- The PROXY header is not authenticated: a listener that accepts it from
  a source it should not lets any client claim any address, which
  defeats rate limits, client classes, access rules and logs keyed by
  address. Enabling it on a port also reachable directly is the usual
  way this goes wrong, and v2's binary format has a history of parser
  bugs elsewhere.

- The deployments in view have other ways to keep the client's address:
  a load balancer that terminates HTTP sends X-Forwarded-For or
  Forwarded, which the trusted proxy settings take, and one that passes
  TCP through can often preserve the source address itself.

## Considered options

- Keep it as planned scope, with a strict design: off by default, per
  listener, required from listed sources and never parsed from others.

- Move it to the future ideas, and take it out of the planned scope and
  the user documentation.

## Decision outcome

Chosen option: move it to the future ideas (section 20.12). Nothing in
Muru reads the PROXY protocol, the user documentation no longer lists it
as coming, and the trusted proxy settings of section 4.13 are the way a
proxy tells the edge the client's address. Should a real need appear,
section 20.12 records the strict design it would need.

### Consequences

- Good: no unauthenticated address source to configure correctly, and
  no binary parser in front of TLS.

- Bad: behind a TCP pass-through load balancer that cannot preserve the
  source address, the edge sees the balancer's address as the client's.

## More information

Sections 4.1, 4.2, 4.13, 15 and 20.12 of the design specification.
