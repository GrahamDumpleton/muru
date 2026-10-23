---
status: accepted
date: 2026-10-23
---

# ADR 0011: Server-rendered dashboard with htmx, Server-Sent Events and uPlot

## Context and problem statement

The built-in dashboard must show live state and charts in a browser. Browsers
cannot speak native gRPC, and a JavaScript build toolchain would be a large
dependency for a Rust and Python project (specification section 10.7).

## Decision drivers

- Building the dashboard needs only cargo.

- Live updates from the same Watch streams the CLI uses.

- Read-only by default, with mutating actions gated, authenticated and
  audit-logged.

## Considered options

- A single-page application (React or similar) built with Node and bundled.

- Server-rendered askama templates kept live with htmx and its Server-Sent
  Events extension over the JSON gateway, with uPlot for charts, both
  vendored and embedded in the binary with rust-embed.

## Decision outcome

Chosen option: the second. A minimal read-only dashboard ships in muructl run
from phase 1 and grows with each phase. If richer client-side interaction is
needed later, individual views can move to a small component library without
changing the gateway.

### Consequences

- Good: no Node toolchain or bundler; one process serves the admin API, the
  gateway and the dashboard; a strict content security policy is easy.

- Bad: complex interactions are harder than in a component framework; two
  vendored JavaScript files must be kept up to date by hand.

## More information

Specification sections 10.6, 10.7 and 14.2.
