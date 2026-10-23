---
status: superseded by ADR 0019
date: 2026-10-23
---

# ADR 0004: Preload on by default in the daemon, off in development mode

## Context and problem statement

A host process can import the application once and fork workers that share
its memory copy-on-write, or each worker can import the application itself
(specification section 6.1).

## Decision drivers

- Fast worker replacement and scaling: a fork rather than an import.

- Shared memory across workers for large applications.

- Applications that cannot be preloaded (import-time threads, connections,
  macOS fork-safety limits) must still work, and development must be simple.

## Considered options

- Preload always on.

- Preload always off.

- Preload on in the daemon, off in muructl run, with a per-release setting.

## Decision outcome

Chosen option: on in the daemon, off in development mode, overridable per
release. The host stays single-threaded before fork, checks for threads
started at import, honours os.register_at_fork, and follows the
gc.disable, gc.freeze, gc.enable sequence to keep pages shared.

### Consequences

- Good: production gets fast forks and shared memory; development avoids the
  macOS Objective-C fork-safety crashes and reloads simply.

- Bad: two code paths for application start-up; preload incompatibilities
  need clear diagnostics and the post_fork hook.

## More information

Specification sections 6.1, 6.2, 6.3, 13.1 and 14.3 (macOS behaviours).

Superseded by [ADR 0019](0019-preload-auto-with-import-check.md), which
replaces the fixed defaults with an automatic mode.
