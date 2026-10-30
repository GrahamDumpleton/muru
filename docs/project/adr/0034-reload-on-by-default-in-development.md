---
status: accepted
date: 2026-10-30
---

# ADR 0034: Reload is on by default in development mode

## Context and problem statement

ADR 0017 made reloading opt-in everywhere, with `muructl run --reload` for
development. `muructl run` is meant to feel like Django's `runserver` and
`flask run` in debug mode, which reload by default, and the project's run
table (specification section 13.1) now gives a `reload` key a meaning only
if there is a default to turn off. Every workshop and tutorial command
carried the flag.

## Decision drivers

- Development should feel like the servers developers come from.

- Production defaults stay predictable: nothing changes for the daemon.

- A key in a file needs a way off for one run.

## Considered options

- Keep reload opt-in with `--reload`, and let the run table set it on.

- Reload on by default in development mode (loopback, not
  `--allow-public`), with `--no-reload` for one run and `reload = false`
  in the run table or `muru.local.toml` for a project or a machine.

## Decision outcome

Chosen option: the second. A development run on loopback watches the
project without a flag; `--reload` stays, as a no-op and as the way to
turn it back on over a file that turned it off. `--allow-public` is not
development mode, so reload is off there unless `--reload` is given, and
the daemon and `muructl serve` never reload. Every reload is still a new
release, never an in-place module reload (ADR 0017's reason stands).

### Consequences

- Good: `muructl run` behaves like the development servers it replaces,
  and the run table's `reload` key has a plain meaning.

- Bad: a run that must not reload, a benchmark say, needs `--no-reload`.

## More information

Specification sections 9.8 and 13.1. Supersedes the reload half of
[ADR 0017](0017-reload-and-error-override-off.md).
