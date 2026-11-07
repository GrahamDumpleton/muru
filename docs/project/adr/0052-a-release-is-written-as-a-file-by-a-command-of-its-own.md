---
status: accepted
date: 2026-11-06
---

# ADR 0052: A release is written as a file by a command of its own

## Context and problem statement

A release reaches the daemon two ways (section 10.4): `muructl deploy`
creates one now from a command line, and `muructl apply` makes the
daemon match the releases a file declares. Nothing wrote the file. It
was written by hand, or exported from a daemon after a deploy. The one
conversion that existed turned a development run into the daemon's
format, behind a flag that ADR 0051 hid, and a run's flags are about
ports, reload and certificates, which are not a release's.

Two more gaps sat beside that one. Nothing showed what a deploy would do
before it did it. And a file could not be applied the way a deploy can
be made: an apply moved traffic to the last declared release as soon as
it was ready, with no way to hold it, return early or say how long the
release it replaced stayed warm.

The specification also said that `config export` writes only the
settings that differ from the defaults, with `--full` for the rest and a
comment naming each one's layer. Export does neither. It writes a
release's settings spelled out, with the interpreter as its resolved
path.

## Decision drivers

- A file kept in version control is the record of a release, so it holds
  the settings, not only a pointer at a directory that can change.

- Writing the file must work where no daemon runs, such as a job that
  writes it for a later step to commit.

- Each command does one thing. A flag that a command accepts and does
  not act on hides a step the person still has to take.

- The settings a release takes are defined once, so that the command
  that writes a release and the one that deploys it cannot come to
  differ.

## Considered options

- A `--print-config` flag on `muructl deploy`.

- A command of its own, `muructl render`.

- Keeping the conversion on `muructl run`.

- A key in a release's table that holds it.

## Decision outcome

Chosen option: a command of its own, with the flags about the act of
deploying given to `muructl apply`.

- **`muructl render APP [SOURCE]` writes a release's table.** It takes
  the settings `muructl deploy` takes for a release and nothing else:
  none of `--hold`, `--no-wait`, `--retain`, `--retain-workers`,
  `--override-freeze` and `--fresh`, and no flag that says where a
  daemon is. It contacts none. Its settings come from the flags, then
  the source's own settings, then the defaults, and never from an
  earlier release, which only one daemon knows. Applying its output
  gives the release a deploy with `--fresh` gives for the same flags.

- **Defaults are left out, and `--full` writes them.** The default
  output holds what the flags and the source's settings give. `--full`
  writes every setting a flag can set, with a comment beside each naming
  where it came from, so the file does not change meaning when a later
  version changes a default.

- **The interpreter is written only when it is named.** A release
  records the path its interpreter resolved to. Resolved where the file
  is written, that path is not the node's. So `python` is written when a
  flag or the source's settings name it, and otherwise left for the
  daemon to find at the apply, where it also builds an environment the
  release needs.

- **`muructl apply` takes the flags about the act.** `--hold`,
  `--no-wait`, `--retain` and `--retain-workers` mean what they mean on
  a deploy and act on every release the apply adds, in every
  application. The plan and the report say `held` of each one. A file
  has no key for them: they describe how the state is reached, and a
  file describes the state.

- **`muructl deploy --dry-run` plans one deploy.** It asks the daemon
  for the id the release would take, each setting with its layer, what
  becomes of the release that serves and what would refuse the deploy.
  It installs nothing. Its exit code is 0 or 1, since a deploy always
  plans a change and the 2 of `apply --dry-run` would say nothing.

- **The conversion of a run is removed.** `muructl run --print-config`
  shows the run's own settings, and now names the interpreter the run
  would use.

- **Export writes a recorded release whole, and the specification says
  so.** `config export` writes a release the daemon has, so that the
  file applies back unchanged: every core setting, and the interpreter
  as resolved. `render` writes a release that does not exist yet, so its
  default leaves the defaults out. Where a recorded release's settings
  came from is what `config show --effective` reports.

`render` is the word for this step where configuration is kept in
version control. A flag on `deploy` was rejected because it would carry
every flag about the act of deploying, and the socket flags, on a
command that then used none of them.

### Consequences

- Good: a release's file is written by a command, from the same flags a
  deploy takes, where no daemon runs.

- Good: a rendered file can be applied held, previewed and promoted with
  a rollout, as a deploy can.

- Good: a deploy can be looked at before it is made.

- Bad: `render` reads the source's settings, so it runs where the source
  is. It cannot write a table for a directory that exists only on the
  node.

- Bad: `--hold` on an apply is all or nothing for the releases that
  apply adds. With `--prune`, where every file is applied together, one
  release cannot be held and another not.

- Bad: a release cannot be held by files alone. A route split that gives
  a new release no traffic lands after the release has been deployed and
  has served for a moment (issue #420).

## More information

Specification sections 10.4 and 13.1, appendices A and B. Replaces the
hidden conversion ADR 0051 kept. Issues #417, #419, #420 and #422.
ADR 0054 gives an application a `serving` key naming the release that
serves, which holds the other releases an apply adds without a flag.
