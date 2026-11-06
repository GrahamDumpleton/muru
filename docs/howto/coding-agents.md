# Coding agents

A coding agent working on a Python web application can run it under the
daemon on the same machine, deploy each change as a release, run the tests
against it, and read what Muru observes when something fails: the
release's state and events, its output and tracebacks, what each worker
thread is doing. When it is done it rolls back or drains.

`muructl mcp serve` gives a coding agent the daemon's operations as
tools over the Model Context Protocol (MCP): deploying releases, moving
traffic, reading events, logs, worker stacks and metrics, and the user
guide. Every command that reports something also prints JSON with
`--json`, for agents that work through a shell.

## Adding Muru to a project

List Muru in the project's MCP configuration. For Claude Code that is
`.mcp.json` at the top of the project:

```json
{
  "mcpServers": {
    "muru": { "command": "muructl", "args": ["mcp", "serve", "--up"] }
  }
}
```

or, from the command line, `claude mcp add muru -- muructl mcp serve --up`.
Other clients have an equivalent file; the command is the same.

The server speaks MCP on its standard input and output and talks to the
daemon over its admin socket, as `muructl` commands do. It needs no
dashboard and no token. `--up` starts the user daemon when none answers,
as `muructl up` does, so the first tool call works on a fresh checkout.
`--socket`, `--system` and the location options choose another daemon, as
for any command.

A [development run](run-settings.md) started from the same directory is
found first, as it is for every command. Its admin socket answers
`status`, `events`, `logs`, `metrics`, `websockets` and `debug`, and
`commands` and `run_command` for [the project's own
commands](commands.md#the-projects-own-commands) with the run's
application as the target, but not the lists or a deploy; stop it, or
give `--socket` the daemon's socket, when the agent is to deploy.

## The development loop

Declare the application in a file beside the code, so that the agent and
the developer deploy it the same way (see
[declaring applications in files](daemon.md#declaring-applications-in-files)):

```toml
# muru.toml
[app.shop.release.r1]
source = "."
entrypoint = "shop.wsgi:application"
python = ".venv/bin/python"
processes = 2
threads = 4
```

Then the agent, through the tools:

1. **Applies** the file once (`apply`), or deploys the working tree as a
   new release after each change (`deploy` with `source` set to the
   project directory). A deploy waits until the release is ready or
   failed, and returns the release.

2. **Runs the tests** against the daemon's listener, `http://127.0.0.1:8080/`
   unless the daemon's configuration names others (`status` lists them).

3. **Reads what happened** when a test fails: `events` for the release's
   state changes and worker faults, `logs` for its output and tracebacks,
   `debug` with `stacks` for what each worker thread is doing and the
   request it serves, `metrics` for counts and latencies.

4. **Moves traffic or cleans up**: `rollback` to the previous release,
   `drain` a release it no longer needs, or a `rollout` with gates when it
   wants Muru to judge the new release against the old.

Steps the project repeats, such as migrating the database, loading
fixtures or a smoke check, belong in [commands](commands.md) declared in
`[tool.muru]`. The agent finds them with `commands` and runs them with
`run_command`, and can declare a new one when it finds itself repeating
a step; it is part of the next release it deploys. Give a secret input in
`secret_inputs`, as an entry of one of the daemon's [secret
stores](secrets.md): `run_command` refuses a secret's value, so it never
passes through the agent.

## The tools

The [MCP tools reference](../reference/mcp-tools.md) lists every tool, its
arguments and the `muructl` command it runs. Each tool runs that command
with `--json` and returns what it prints, so the shape of a result is the
one the [JSON output reference](../reference/json-output.md) gives, and a
failure returns the sentence `muructl` would print, as a tool error.
Stopping and upgrading the daemon, running code in workers, and one-off
commands (`muructl exec`) are not offered as tools: `run_command` runs
only the commands an application declares.

## Limiting what the agent may do

- `--read-only` offers only the tools that read: an agent can watch and
  diagnose but not deploy or move traffic.

- `--allow deploy,drain` offers the tools that read and, of those that
  change things, only the ones named.

Both are enforced by the server: a tool left out is not listed and cannot
be called.

Whoever launches the server has the access the admin socket gives, which
for a developer's own user daemon is everything. Do not point an agent at a
shared system daemon unless that is intended.

## The skill

The tools let an agent operate Muru; the skill teaches it what a release
is, what each operation does to traffic, the development loop, and what to
check after each step. It is embedded in `muructl`, matches its version,
and lives in the project, so it travels with the code:

```
muructl mcp skill install          # writes .claude/skills/muru/SKILL.md
muructl mcp skill install --dir .agents/skills    # another client's skills directory
muructl mcp skill status           # current, missing or out of date; exits 1 unless current
muructl mcp skill print            # the skill on standard output
```

Commit the skill with the project. After upgrading `muructl`, run `install`
again; `status` in a `just` recipe or CI says when that is due. The MCP
server checks too: when the project's copy is missing or differs from the
`muructl` serving it, the instructions it gives the agent at the start of
a session say so and name the command, and the `skill_status` tool answers
the same question at any time. The skill works through the MCP tools when
the agent has them and through `muructl --json` when it does not.

## Without MCP

An agent with a shell can run `muructl` directly. Add `--json` to any
command that reports something, and it prints the admin API's answer as
JSON, or `{"error": ...}` when it fails, with the same exit status.
