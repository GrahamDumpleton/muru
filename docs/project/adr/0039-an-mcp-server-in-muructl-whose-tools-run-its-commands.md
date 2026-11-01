---
status: accepted
date: 2026-11-01
---

# ADR 0039: An MCP server in muructl whose tools run its own commands

## Context and problem statement

A coding agent developing a Python web application should be able to use
Muru inside its loop: deploy the working tree as a release, test it, read
what Muru observes, and roll back or drain (issue #197). An agent with a
shell can run `muructl`, but only reads text, and an agent without one has
nothing. The JSON gateway needs the dashboard and a token and is mostly
read-only. Section 10.11 of the specification describes the server; this
record says how it is built.

## Decision drivers

- One shape for a result, whether an agent asks through MCP, a script runs
  `muructl --json`, or the documentation describes it.

- No second distribution channel or package to keep in step with muructl.

- Few tools, since most clients put every tool description into the
  agent's context on every turn.

- Limits on what an agent may change that hold on the server's side, since
  the protocol's read-only and destructive annotations are advisory.

## Considered options

- A server over the JSON gateway, in Python or Rust: lighter, but it needs
  the dashboard and a token, and cannot deploy, apply or drain.

- A server in muructl speaking gRPC to the admin API itself, with its own
  code for each tool's request and result.

- A server in muructl whose tools run muructl's own commands with `--json`
  and return what they print.

- No server: the `--json` output and a skill alone, enough for agents with a
  shell.

## Decision outcome

Chosen option: a server in muructl, `muructl mcp serve`, on standard input
and output with the official Rust SDK (rmcp), whose tools run muructl's own
commands with `--json` against the same daemon.

- **Tools run commands.** Each tool builds a muructl command line from its
  arguments, runs the executable that serves it with `--json` and the
  server's socket and location options, and returns what the command
  prints, or its error's sentence as a tool error. The result's shape, the
  argument checks and the error messages are the command's by construction,
  and the JSON output reference describes both. A command costs a process
  start of a few milliseconds, small beside what a deploy or a test run
  takes.

- **Task-shaped tools.** About twenty-five tools, each a task an agent has
  (list one kind of object, show a release, deploy, roll back, look at the
  workers) rather than one per remote call. Stopping, shutting down and
  upgrading the daemon, and running code in workers, are not offered.

- **Limits on the server.** `--read-only` leaves out every tool that
  changes something and `--allow` keeps only the ones named; a tool left
  out is neither listed nor callable.

- **Streams as snapshots.** Following events or logs becomes a bounded
  batch the agent asks for again, since a tool call returns once.

- **The guide in the binary.** A docs tool returns user guide pages
  embedded at build time, as the dashboard embeds its assets, so the agent
  reads the documentation of the muructl it talks to.

- **Local only for now.** Whoever launches the server has the admin
  socket's access, which for a developer's user daemon is the intent.
  Remote agents wait on the TCP admin listener and its access model (issues
  #146 and #187), with a streamable HTTP transport.

- **A skill the project carries**, for what the tools cannot teach (what a
  release is, that promote moves traffic, what to check after a deploy):
  printed and installed by muructl from a copy embedded in it (issue #255).

### Consequences

- Good: one implementation of every operation, one shape for every
  result, and no drift between the CLI, MCP and the reference pages.

- Good: an agent without a shell gets a typed, discoverable surface, and a
  developer can limit what it may change.

- Bad: a tool's result can carry no more than the command prints; a tool
  that needs more needs the command to print it first.

- Bad: each call starts a process, and the server depends on finding its
  own executable.

## More information

Specification section 10.11, Appendix A. Issues #197, #254 and #255.
