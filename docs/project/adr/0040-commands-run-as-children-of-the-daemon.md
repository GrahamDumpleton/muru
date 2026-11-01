---
status: accepted
date: 2026-11-01
---

# ADR 0040: Commands run as children of the daemon with the release's settings

## Context and problem statement

`muructl exec RELEASE -- COMMAND` is the supported way to run a one-off
command, a database migration above all, with what a release's workers
see: its interpreter, environment and secrets, user and source directory
(sections 6.8 and 9.7, issue #122). Named commands an application
declares (issue #96) are the same thing with a name and inputs. Something
has to start the process, and it has to have the release's settings to
hand.

## Decision drivers

- A command sees exactly what the release's host sees, resolved the same
  way, so a migration run with `exec` runs against the database the
  workers use.

- It works whatever state the release is in: serving, held, retained,
  drained or stopped, scaled to zero with its host gone, or never started
  under this daemon.

- Its output reaches the caller as it is written, and it ends with the
  caller unless it is told otherwise.

- Nothing new in the host or its control protocol for the first form.

## Considered options

- The host spawns the command, asked over its control connection, and
  relays the output back.

- The daemon spawns the command itself, with the settings it resolves to
  start the release's host.

- muructl spawns the command in the caller's own process, from the
  release's recorded settings.

## Decision outcome

Chosen option: the daemon spawns the command, with the settings it
resolves to start the release's host, and the admin API streams its
output (`RunCommand`).

- **The same settings as the host.** The daemon resolves the release's
  user, its environment with every secret delivered as the host gets it,
  its interpreter (the built environment's, for a release that builds
  one), the runtime library on PYTHONPATH and the bytecode directory,
  then starts the command in the release's source directory. The
  interpreter's directory goes first on PATH, VIRTUAL_ENV is set when it
  is a virtual environment, and a command whose program is `python` or
  `python3` runs the release's interpreter itself, since a system
  interpreter's directory need not hold one named `python`. MURU_APP,
  MURU_RELEASE and MURU_RELEASE_DIR name what it runs with.

- **Outside the daemon's loop.** The daemon only resolves the settings;
  the admin API's task starts the process and relays it, so a long
  migration holds nothing up.

- **No shell, no input.** The command is an argument vector, run without
  a shell, with standard input closed and no terminal.

- **Ends with its caller.** The command runs in a session of its own. When
  the caller goes away, or the command outlives its timeout, the session
  is sent SIGTERM and, ten seconds later, SIGKILL. The exit status is the
  command's, or 128 plus the signal that killed it.

- **Recorded.** Output lines are appended to the release's log, prefixed
  `[command NAME]` (`exec` for a one-off command), and `command.started`,
  `command.finished` and `command.failed` events record each run.

- **Not offered over MCP.** A one-off command is arbitrary code, like
  `debug exec`, so the MCP server does not offer it.

### Consequences

- Good: no change to the host or its protocol, and a release need not be
  running for a command to run with it.

- Good: the settings are resolved by the code that starts hosts, so a
  command cannot drift from what the workers see.

- Bad: a command does not get the preloaded application, as a process
  forked from the host would, and pays for its own imports.

- Bad: a container release (section 8.5) runs its host inside the
  container and the daemon outside it, so a command for one has to be
  started in the container, through its runtime or its host, when
  container releases are built; under `muructl serve` the process that
  serves is the one that runs it.

## More information

Specification sections 6.8, 9.7 and 9.12, Appendix B. Issues #96 and
#122.
