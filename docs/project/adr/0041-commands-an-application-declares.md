---
status: accepted
date: 2026-11-01
---

# ADR 0041: Commands an application declares, with checked inputs

## Context and problem statement

Operating an application means running the same steps again and again:
a migration before a deploy, a backup, creating an account, loading
fixtures, a smoke check. Today each person or script that runs one has
to know the command line, which release to run it with, and where its
secrets come from, and a coding agent has to rediscover all of it each
session. Issue #96 asked for applications to declare these steps to
Muru, by name and with the values each needs, so they are run the same
way by an operator, a deploy script or an agent through MCP. ADR 0040
says how a command's process runs; this record says how commands are
declared, given inputs and offered.

## Decision drivers

- A step is versioned with the code it acts on, so a release runs the
  migration its own code carries.

- Inputs are checked before anything runs, and a secret input never shows
  in a command line, an event, a log or an error.

- An agent can find and run the declared steps, and add one, without
  being able to run arbitrary code or see a secret.

- No new language: argument vectors and Python callables, as the rest of
  the configuration uses.

## Considered options

- Commands in the daemon's configuration or applied files only, managed
  apart from the code.

- Commands in the project's `[tool.muru]` table, fixed with each release,
  overridable by a declared release's table.

- Shell snippets with variable interpolation.

## Decision outcome

Chosen option: commands in `[tool.muru.commands.NAME]`, fixed with each
release, overridable by a declared release's table, as argument vectors
(`run`) or Python callables (`call`), with declared inputs.

- **Part of the release.** Commands are resolved and checked when the
  release is created and are immutable with it. A new or changed command
  takes effect with the next deploy; an agent that records a new step
  deploys before it can run it, and the step is then in version control
  with the code.

- **Placeholders, not a shell.** `{name}` in an argument or an `env`
  value takes an input's value, as a whole substitution with no further
  parsing, so a value cannot become two arguments or a shell construct.

- **Checked inputs.** Required, default, pattern and secret, checked by
  the daemon before the process starts. A secret input may only be used
  in `env`, since another user of the machine can read a process's
  arguments, and its value is shown as `***` in events and never in
  errors.

- **Secrets by reference.** A run can name a secret store entry as
  `STORE/KEY` for an input, resolved by the daemon. The MCP tool refuses
  a secret input given as a value, so an agent can run a command that
  needs a password without ever holding it.

- **Callables run fresh.** A `call` command is a new interpreter that
  imports the function and calls it with the inputs as keyword
  arguments, through a small runner in the runtime library. Forking from
  the preloaded host would be faster but needs the host running and a new
  host operation; a fresh process works for every release state.

- **Exclusive commands and last runs.** `exclusive` allows one run at a
  time per application, held by the daemon for the life of the process;
  the store keeps each command's last run for `muructl command list`.

- **Over MCP**, listing and running declared commands are tools;
  `muructl exec` is not, so an agent runs only what the application
  declares.

### Consequences

- Good: the steps of operating an application live beside its code, are
  discoverable (`muructl command list`, the `commands` tool) and run the
  same way from every caller.

- Good: an agent can be trusted with a command that needs a secret.

- Bad: a command cannot be changed without a release, which is slower
  when iterating on the command itself; `muructl exec` covers that.

- Bad: running a command at a point in a release's life (before its
  workers fork, before a promotion), from the dashboard or on a schedule
  needs more work, tracked in issues of its own.

## More information

Specification section 9.12, Appendices A to C, section 10.11. ADR 0040.
Issue #96.
