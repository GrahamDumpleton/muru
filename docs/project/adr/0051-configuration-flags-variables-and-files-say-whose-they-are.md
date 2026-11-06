---
status: accepted
date: 2026-11-06
---

# ADR 0051: Configuration flags, variables and files say whose they are

## Context and problem statement

Muru reads three kinds of configuration file: the daemon's, which
describes a machine and any number of applications; a project's, the
`[tool.muru]` table of its `pyproject.toml` or a `muru.toml` beside its
code, which describes one application and ships with it; and one
developer's `muru.local.toml`. The first two hold different keys and
refuse each other's.

Adding commands that run for a project with no daemon (ADR 0050) showed
that the names around these files had grown apart from what they name.

- `--config` named the daemon's file on every command that can reach a
  daemon, about 65 of them, where all but the few that start one accept
  it and never read it. `muructl serve --config` named something else, a
  file of serve's keys at its top level, the only settings file of that
  shape: everywhere else serve's settings are a `[serve]` table. A
  project command needed that file and could not call its flag
  `--config`.

- `MURU_CONFIG` named the daemon's file without saying so, beside
  `MURU_SERVE_CONFIG`.

- `--print-config` wrote a daemon-format file on `muructl run` and the
  command's own resolved settings on `muructl serve`.

- The daemon's file and a project's are both `muru.toml` by default. The
  guides told a project to keep a daemon-format file for `muructl apply`
  beside its code under that name, where it is read as the project's
  settings and refused (#408).

- A project with both a `muru.toml` and a `[tool.muru]` table uses the
  first and never reads the second, which nothing said (#409).

## Decision drivers

- A name says which file it means, wherever it is read.

- A flag is offered only where it has an effect.

- One file format for a project's settings, wherever the file is.

- No merge rules between files that hold tables.

## Considered options

- `--daemon-config` and `--serve-config` on every command, with no bare
  `--config`.

- `--config` only on the commands that belong to one mode, where the
  command says whose file it is, with the variables carrying the mode.

- For serve's file: lay it over the project's file key by key; replace
  the project's file outright; or delete it and rely on the variables.

- Rename the daemon's default file to `muru.daemon.toml`.

## Decision outcome

Chosen option: `--config` only where a command belongs to one mode, and
one project settings file wherever it lives.

- **`--config` on the commands of a mode.** The daemon's own commands
  take the daemon's file with the other location flags: the `muructl
  daemon` commands, so that one set of flags starts, stops and reloads a
  daemon, `muructl up`, `muructl mcp serve --up`, and `muructl secret`,
  which reads it. A command that only talks
  to a running instance takes `--socket`, `--run-dir` and `--system`. On
  `muructl serve`, and on a command run for the project, `--config`
  names a project settings file.

- **The variables carry the mode.** A variable has no command around it,
  so the daemon's is `MURU_DAEMON_CONFIG` beside `MURU_SERVE_CONFIG`. A
  daemon started with `MURU_CONFIG` set and the new name not is refused,
  since it would otherwise start without its configuration.

- **Serve's file is a project settings file.** `muructl serve --config
  FILE`, or `MURU_SERVE_CONFIG`, reads FILE in place of the file beside
  the code: in the form of a `muru.toml`, with serve's settings under
  `[serve]`. It replaces the project's file outright. Relative paths in
  it are the project directory's. The top-level serve file format is
  gone. One setting is still changed over it with a `MURU_SERVE_*`
  variable or a flag.

- **`--print-config` shows what a command would use.** On `muructl run`
  and `muructl serve` it prints the resolved settings in the shape of
  the project's file, each with the layer that set it. The conversion of
  a run to the daemon's format is kept behind a hidden
  `--print-daemon-config`. Its output restates every resolved value in a
  release's table, where it then stands over the project's own file, and
  the path from a development run to the daemon wants working out before
  a flag is offered for it.

- **The daemon's default file keeps its name.** No project's file lives
  in `/etc/muru` or a user's configuration directory, so `muru.toml`
  there is not ambiguous. A daemon-format file kept beside a project's
  code is named `muru.daemon.toml` by convention, and a project's
  settings file refused for a key of the daemon's format says so.

- **Both project files.** `muru.toml` is used in place of the
  `[tool.muru]` table and the two are never merged. `muructl run`,
  `serve`, `check` and `deploy` say so once when a project has both.

### Consequences

- Good: `--config` appears only where it is read, and a project command
  takes the same flag and variable `muructl serve` does, so a Job and a
  Deployment read the same settings.

- Good: serve's settings are a `[serve]` table everywhere, and
  `--print-config` output can be saved and used as the file `--config`
  names.

- Bad: a mounted settings file restates what the image's file declared,
  the entrypoint and the commands included, so the two can drift.

- Bad: the daemon's file and a project's still share the name
  `muru.toml`, told apart by where they are.

- Bad: nothing offered converts a development run to the daemon's
  format. A release's table needs only its `source`, and `muructl
  deploy` needs no file.

## More information

Specification sections 6.8, 10.4, 13.1, 13.8 and 13.9. Amends ADR 0044,
whose layers included the file `MURU_SERVE_CONFIG` names as one of
serve's keys. Issues #416, #408, #409 and #292.
