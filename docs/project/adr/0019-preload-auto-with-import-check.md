---
status: accepted
date: 2026-10-23
---

# ADR 0019: Preload defaults to an automatic mode with an import check

## Context and problem statement

ADR 0004 made preload the default in the daemon and turned it off in
development mode. Reviewing that against how applications are written showed
two problems. First, gunicorn and uvicorn do not preload by default, so many
applications have never been run under fork-after-import, and the ones that
do work at import time (open a database or Redis connection, create a Kafka
producer or gRPC channel, start a telemetry thread, run a warm-up query)
fail after fork in the worst way: intermittently, in production, with shared
sockets corrupting each other's traffic. Second, with preload off in
development, that failure could only be discovered on deployment.

## Decision drivers

- The design depends on preload for fast worker replacement, scale-up by
  fork, retained releases at a warm floor and shared memory, so dropping it
  would weaken much of section 7.

- An application must not silently break in production because of a default.

- Development and production should behave the same way, so problems show
  up on the laptop.

- uWSGI, which preloads by default, shows that the model works at scale when
  the failure modes are understood; gunicorn's opt-in shows that most
  applications work under preload once they are written for it.

## Considered options

- Keep preload on by default and rely on the thread warning and the
  post_fork hook (ADR 0004).

- Preload off by default, opt-in as in gunicorn: safe, but gives up the
  benefits for everyone, including the majority whose imports are fork-safe.

- An automatic mode: preload, check what the import left behind, and fall
  back to per-worker imports when anything would be shared or lost across
  fork, with explicit true and false settings for people who want to force
  either behaviour.

## Decision outcome

Chosen option: the automatic mode, as the default in the daemon and in
development mode alike. After the import and the on_load hooks the host
compares its descriptor table, thread count (including native threads),
child processes and event loops with their state before the import. If
nothing is found, it forks. If something is found, it warns, naming the
descriptor, thread or module, raises an event, and runs the release with
per-worker imports. preload = true turns a finding into a release failure,
for applications that have been made fork-safe and want regressions caught.
preload = false skips the check.

### Consequences

- Good: safe by default for the applications most at risk; the benefits of
  preload for the majority that are fork-safe; the same behaviour and the
  same warning in development and production; a clear path (fix the import
  or use post_fork, then set preload = true) to the fast model.

- Bad: a host that falls back has paid for an import it will not use (one
  extra import at release start); the check can miss state that is not
  visible as a descriptor, thread, child or loop, such as a lock held by a
  library, so the post_fork hook and the documentation on fork safety still
  matter; the check adds a small amount of platform-specific code (procfs on
  Linux, Mach task information on macOS).

## More information

Specification sections 6.1 (preload modes), 6.2 (the import check), 13.1,
14.3, 15.2 and Appendix C. Supersedes [ADR 0004](0004-preload-defaults.md).
