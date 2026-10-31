---
status: accepted
date: 2026-10-23
---

# ADR 0007: uv is an external program, never linked in

## Context and problem statement

Muru provisions interpreters and builds environments. It could embed a
resolver, shell out to a tool, or leave provisioning to the user
(specification section 8.4).

## Decision drivers

- uv already does interpreter installation, resolution and fast environment
  builds well, including free-threaded builds.

- Provisioning is never on the request path.

- Muru must not shadow or reconfigure a user's own uv.

## Considered options

- Link a resolver into muructl.

- Require users to prepare interpreters and environments themselves.

- Run uv as a separate program found through a lookup order, with an optional
  muructl[uv] extra and a managed, checksum-verified copy as a fallback.

## Decision outcome

Chosen option: external uv. muructl runs it with UV_NO_CONFIG and its own
cache and install directories under the state directory, offline with a
local wheel directory when configured, and checks uv --version at start-up.
Prebuilt release directories need no build step, so uv is optional.

### Consequences

- Good: no resolver to maintain; isolation from user configuration; the
  runtime library can still be installed by unpacking a wheel without pip.

- Bad: a version dependency on an external tool, and features that vary with
  its version must be reported rather than assumed.

## More information

Specification sections 8.1 to 8.4 and 17.1. The isolation sentences are
softened by [ADR 0036](0036-install-from-locks-and-build-what-is-missing.md):
development mode runs the developer's uv as it is, and the daemon isolates
its directories but passes `UV_CONFIG_FILE` rather than `UV_NO_CONFIG` for
builds, so the project's own `[tool.uv]` table applies. External uv stands.
