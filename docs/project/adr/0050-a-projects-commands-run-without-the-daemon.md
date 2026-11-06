---
status: accepted
date: 2026-11-06
---

# ADR 0050: A project's commands run without the daemon, through what serves it or by muructl itself

## Context and problem statement

ADR 0040 has the daemon start every command, with the settings it
resolves to start a release's host, and rejected muructl starting one in
the caller's own process. ADR 0041 adds the commands an application
declares. Both assume a daemon and a release.

Two places have neither (#292). A container runs `muructl serve`, whose
admin socket refused commands, so the Kubernetes guide ran a migration
as a Job calling the image's interpreter directly, past the declared
command with its inputs, its timeout and the project's environment
variables. A development run has no release either, so a migration was
run by hand with `uv run`, a different way from production.

Who starts a command there, and how does a caller say which form it
wants?

## Decision drivers

- A migration in a container platform runs before the new image serves,
  in a Job, an init container or a Helm hook, where nothing of the
  application is running to ask.

- A command asked for while the application is being served, in
  development or with `kubectl exec` into a serving pod, belongs with
  that process: its output in the same log, and `exclusive` holding
  there.

- A Job and a Deployment made from one image and environment must find
  the same project, interpreter and runtime library.

- One command to learn. The daemon's form stays explicit, since it runs
  a release's own code and not what is in the current directory.

## Considered options

- A flag on each mode: `muructl run --command NAME`, `muructl serve
  --command NAME`, and `muructl command run APP NAME` for the daemon.

- A separate form that only ever runs on its own, never through a
  running process.

- One `muructl command run`, `muructl command list` and `muructl exec`,
  whose target is set by whether an application is named.

## Decision outcome

Chosen option: one set of commands. With an application named, the
daemon runs the command, as ADR 0040 has it. With none, the command is
one of the project in the current directory or `--chdir`, and runs
through whatever serves that project when something does, and by muructl
itself otherwise.

- **Through a running process.** The admin socket of `muructl run` and
  `muructl serve` answers `RunCommand` and `ListCommands`. muructl looks
  for a `muructl serve` in the run directory, then a development run
  started from the project, and passes over one that no longer answers.
  The command gets the interpreter, runtime library and project
  environment the workers were started with, its output joins the
  process's own, and its events are printed as that process's others
  are.

- **By muructl itself.** With nothing serving, muructl resolves the
  project, the interpreter and the runtime library with the code
  `muructl serve` and `muructl run` use, starts the command and exits
  with its status. The settings are the few a command needs: `--chdir`,
  `--python`, `--runtime-dir` and `--serve-config`, each also read from
  where `serve` reads it. The image's runtime directory is used when a
  setting names it or it holds a library; otherwise the library comes
  from where a development run takes it. No environment is built.

- **The same runner.** All three forms prepare a command and hand it to
  the code that starts it, relays its output, enforces its timeout and
  stops its session. Only where a run is recorded differs, which the
  runner takes as a record: the daemon's store and events, the serving
  process's output, or nothing.

- **The declarations are read at each run** from the project's file, so
  a command added to it is found without restarting what serves.

- **Standard input stays closed** in every form, so a command behaves
  the same whether or not something happens to be serving the project.

- **`exclusive` is not refused without the daemon.** It holds within one
  serving process. A command muructl runs itself is a single run, and
  keeping it to one at a time is up to what starts it: a Job with
  `parallelism: 1`, a Helm hook, the migration tool's own lock.

- **Secret store inputs are the daemon's.** `--secret-input` is refused
  without it, naming `--input-stdin`. A container's secrets are already
  in its environment or mounted, where a command sees them as the
  application does.

- **The settings file flag is `--serve-config`**, not `--config`, which
  on these commands already names the daemon's configuration file.

### Consequences

- Good: a Job runs `muructl command run migrate`, with the declared
  command's inputs, timeout and environment, and a developer runs the
  same thing.

- Good: one runner, so the three forms cannot drift in how a command is
  started, timed out or stopped.

- Bad: a command with no application named silently changes where its
  output is recorded when a run of the project is up. The command, its
  interpreter and its environment are the same either way.

- Bad: the last run of a command is kept only under the daemon, and
  `exclusive` does not hold across the pods of a Deployment.

- Gates that run a command before a release's workers fork, and
  `muructl command run --gates`, remain with #266.

## More information

Specification sections 9.12, 13.1 and 13.9. Amends ADR 0040, which
chose the daemon over muructl as what starts a command; that holds for a
named application. Issues #292, #270 and #266.
