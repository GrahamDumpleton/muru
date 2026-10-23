---
status: accepted
date: 2026-10-23
---

# ADR 0014: Managed HTTP server releases, in a limited form, for migration cutovers

## Context and problem statement

Existing deployments run gunicorn, uvicorn, Granian or mod_wsgi-express.
Moving them to Muru in one step means changing the server and the deployment
model at once (specification section 9.9).

## Decision drivers

- A migration path with a canary between the old server and native workers,
  with zero failed requests.

- Keep Muru's scope on Python applications: general management of non-Python
  servers is a non-goal.

## Considered options

- Out of scope: migrate by replacing the server directly.

- Full support: autoscaling, socket activation and many server profiles.

- Limited support: fixed instance counts, server profiles for gunicorn,
  uvicorn, Granian, mod_wsgi-express and a generic server, full traffic
  management, no Python-specific features (no preload by Muru, groups, mounts,
  soft timeouts, memory policy or descriptor passing).

## Decision outcome

Chosen option: limited support in phase 3. A managed HTTP release is a normal
backend in traffic policies, so canaries, A/B splits, stickiness, previews
and rollback work between it and native releases of the same application.
Instance autoscaling and more profiles remain future ideas.

### Consequences

- Good: a cutover demonstration becomes a phase 3 exit criterion; existing
  deployments can adopt Muru's traffic management first.

- Bad: requests on this path are re-serialised as HTTP with strict
  normalisation; two process managers coexist inside each instance.

## More information

Specification sections 4.13, 9.9, 14.5 and 20.1.
