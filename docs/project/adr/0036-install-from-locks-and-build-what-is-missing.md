---
status: accepted
date: 2026-10-31
---

# ADR 0036: Install from locks, build what is missing, change what exists only when asked

## Context and problem statement

Section 8 lets Muru provision an interpreter and an environment through uv,
but did not say when it does so on its own. `muructl run` on a fresh clone
with a `uv.lock` and no `.venv` fell through to `python3` on PATH and failed
on the first import; the daemon's build step was specified but not its
default; and the same provisioning would be written three times, in `run`,
in the controller and in the image build. The questions were whether
provisioning happens by default or by opt-in, in development, under the
daemon and in containers, and how far muructl may go in changing a
developer's machine (specification sections 8.1, 8.2, 8.4 and 13.1).

## Decision drivers

- A fresh clone followed by `muructl run` should work, as `uv run` has
  made normal.

- A developer's environment is theirs: an experiment installed by hand
  must not be undone by a server start, and a reload must not stall on a
  package install nobody asked for.

- The environment a node builds must be the one the developer tested, so
  nothing on a node may resolve dependencies.

- Development mirrors the developer; the daemon builds for deployment, as
  another user, never root (ADR 0026).

- One vocabulary. `sync` is uv's word; Muru should have one verb for
  provisioning as a command, a flag and a setting.

## Considered options

- Never provision from `run`; fail with a message naming `uv sync`. The
  same network access and the same directory one command later.

- Provision on every start, as `uv run` does, with a flag to turn it off.

- Build what is missing and change what exists only when asked, through
  one knob read in every mode, with provisioning as a command of its own
  that `run`, the daemon and the image build call.

- Alongside any of these, a second builder for machines without uv:
  `python -m venv` and pip from an exported `pylock.toml` or
  `requirements.txt`, behind the same settings.

## Decision outcome

Chosen option: the third.

- `muructl build [PATH]` is the one provisioning procedure: resolve the
  interpreter, build the environment from the lock, install the runtime
  library, exit. `muructl run` is `build` then serve, the daemon's
  `Building` state is `build --deployment` into the state directory, and
  the generated Dockerfile's build stage is `build --deployment --deps-only`
  then `build --deployment`. `muructl check` validates what `build`
  prepared and has no side effects.

- One knob, `build.when`, with the same values in development and under
  the daemon: `auto` (the default) builds a missing environment, leaves an
  existing one alone at start, and while running builds on a change it
  watches the developer make, to `uv.lock`, `.python-version` or
  `requires-python`, before the new release starts (a change to
  `pyproject.toml` alone reloads without a build, since the lock did not
  change); `always` also builds on every start; `never` builds nothing. At
  start an environment behind the lock is reported, from `uv sync
  --frozen --dry-run`, never changed. `muructl run
  --build [MODE]` sets it for one run, a bare `--build` meaning `always`;
  `[tool.muru.build]` sets it for a project and `muru.local.toml` for one
  machine; `muructl deploy --build MODE` for one deploy. There is no
  `--sync` and no `--no-build`.

- Muru installs from locks and never resolves. A project without a lock is
  told the command that makes one. Building needs `pyproject.toml`,
  `uv.lock` and uv. The second builder is left out until there is demand
  for it: uv is a single binary that `muructl tools install uv` fetches,
  so the builder would only ever serve a node where fetching uv is
  forbidden, which can deploy a prebuilt directory or build in CI instead.
  A machine without uv still runs on an existing environment, and nothing
  that works today stops working.

- A missing interpreter is installed where a missing environment would be
  built, and nowhere else. In development the build's `uv sync` downloads
  it into the developer's own uv directory; muructl never calls `uv python
  install` there. The daemon installs explicitly into its state directory
  when downloads are allowed. Containers never do, and without uv a
  request cannot be resolved at all.

- Development uses the developer's uv as it is: no private cache, install
  directory or configuration. The daemon isolates its directories but keeps
  the project's own `[tool.uv]` configuration, passing `UV_CONFIG_FILE`
  rather than `UV_NO_CONFIG` for builds, because `--no-config` discards the
  project's indexes and sources (verified with uv 0.13.5). Built
  environments live in the state directory, STATE/envs/APP/RELEASE, not the
  run directory, since a retained release still needs its environment
  after a daemon restart.

- Under `--allow-public`, and always in a container, nothing is built: the
  message names `muructl build` or the Dockerfile line.

### Consequences

- Good: a clone runs with one command; a stale environment is never
  changed behind a developer's back; the node runs exactly the lock the
  developer tested; the provisioning logic exists once and `check` has no
  side effects; the vocabulary is `build` everywhere.

- Bad: a developer who pulls a new lock while the server is stopped is
  told at the next start that the environment is behind, and must ask for
  the rebuild with `--build`; a first `muructl run` on a clone may download
  packages and a Python, announced as it happens; a machine without uv
  cannot build or resolve a version request and is told to install it.

## More information

Specification sections 8.1 to 8.4, 9.1, 13.1 and 13.8. Softens the
isolation sentences of [ADR 0007](0007-uv-as-external-tool.md); external uv
stands. Decided in issue #237 on 2026-10-31; the implementation issues are
#124, #125 and #235. The builder without uv is parked as #236.
