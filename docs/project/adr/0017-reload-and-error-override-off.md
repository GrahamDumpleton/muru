---
status: accepted
date: 2026-10-23
---

# ADR 0017: Reload and error override are off by default

## Context and problem statement

Two conveniences are common in other servers: reloading on file changes, and
replacing an application's own error responses with server-generated pages.
Both change behaviour in ways that surprise production operators
(specification sections 4.12 and 9.8).

## Decision drivers

- Production defaults must be predictable.

- Development should still be convenient with one flag.

- Every reload is a new release, never an in-place module reload.

## Considered options

- Reload on by default with watch mode, as development servers do.

- Reload off by default; touch and watch modes opt in; muructl run --reload
  for development.

- Error override on by default for 5xx responses.

- Error override off by default, with when conditions and a per-response
  opt-out header when enabled.

## Decision outcome

Chosen options: the second and the fourth.

### Consequences

- Good: no surprise restarts or replaced API error bodies in production; the
  development experience is unchanged with --reload.

- Bad: operators who expect automatic reloads must configure a mode.

## More information

Specification sections 4.12, 9.8 and 13.1.
