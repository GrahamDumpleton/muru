---
status: accepted
date: 2026-10-23
---

# ADR 0003: Releases are immutable and configuration changes are releases

## Context and problem statement

Deploys, canaries and rollbacks need a unit that can run side by side with
its predecessor and be reasoned about as a whole. Application servers usually
reload in place instead (specification sections 3 and 9).

## Decision drivers

- Zero-downtime deploys with instant rollback.

- One audit trail for code and configuration.

- Simple traffic policy: a policy decides which release serves, nothing else.

## Considered options

- Mutable application objects reloaded in place, as gunicorn and uvicorn do.

- Immutable releases (source, interpreter, environment, entrypoints and
  settings fixed at creation), with traffic policy the only thing deciding
  whether a release serves. Scale and limits are the only live changes.

## Decision outcome

Chosen option: immutable releases. A configuration change, a secret rotation
or a reload all create a new release that gets the same drain, canary and
rollback as a code change. There is never an in-process module reload.

### Consequences

- Good: every deployment strategy is a sequence of policy changes plus
  lifecycle management; rollback is restoring a policy to a retained release.

- Bad: two releases overlap during a deploy, roughly doubling memory, which
  the node budget must account for; shared state (databases, caches) must be
  compatible across the overlap, which Muru can only document, not enforce.

## More information

Specification sections 3.1, 9.1, 9.7, 9.8 and 13.6.
