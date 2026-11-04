# MCP tools

The tools `muructl mcp serve` offers a coding agent (see
[coding agents](../howto/coding-agents.md)). Each runs the `muructl` command
shown with `--json` against the same daemon and returns what it prints, as
the [JSON output reference](json-output.md) describes; a command that fails
makes the tool return its error's sentence as a tool error. `--read-only`
leaves out every tool that changes something, and `--allow` keeps only the
ones it names.

| Tool | Kind | Runs | Arguments |
| --- | --- | --- | --- |
| `status` | reads | `muructl status` | none |
| `list` | reads | `muructl app list`, `release list`, `host list`, `route list`, `upstream list`, `rollout status` or `overlay list` | `what` (apps, releases, hosts, routes, upstreams, rollouts, overlays), `app` |
| `release` | reads | `muructl release show` | `release` (app@release) |
| `commands` | reads | `muructl command list` | `target` |
| `events` | reads | `muructl events` | `after` (a sequence number) |
| `logs` | reads | `muructl logs`, the lines returned as an array | `target`, `lines` (default 100, at most 1000), `access` |
| `debug` | reads | `muructl debug status`, `stacks`, `tasks`, `memory` or `inspect` | `what`, `target`, `worker` |
| `scale_status` | reads | `muructl scale status` | `target` |
| `config` | reads | `muructl config show` or `config diff` | `action` (show, diff), `app`, `effective`, `file` |
| `metrics` | reads | `muructl metrics`, the samples kept to those whose name starts with `prefix` | `prefix` |
| `websockets` | reads | `muructl websockets` | `app`, `list` |
| `overlay_preview` | reads | `muructl overlay preview` | `name` |
| `skill_status` | reads | `muructl mcp skill status` | none |
| `docs` | reads | The user guide embedded in `muructl`: the list of pages, or one page's Markdown | `page` |
| `deploy` | changes | `muructl deploy` | `app`, `source`, `entrypoint`, `python`, `hold`, `wait` (default true) |
| `promote` | changes | `muructl rollout bluegreen` | `release` |
| `rollback` | changes | `muructl rollback` | `app` |
| `drain` | changes | `muructl drain` | `release` |
| `rollout` | changes | `muructl rollout canary`, `pause`, `resume`, `promote` or `abort` | `action`, `release` (canary), `app` (the others), `steps`, `hold` |
| `scale` | changes | `muructl scale` | `target`, `group`, `processes`, `unpin` |
| `run_command` | changes | `muructl command run`, the output returned as an array of lines (the last 500) with `exit_code` and `message`, as a tool error when the exit status is not 0 | `target`, `name`, `inputs` (an object), `secret_inputs` (an object of STORE/KEY references) |
| `restart` | changes | `muructl restart` | `target`, `worker`, `all`, `force` |
| `overlay` | changes | `muructl overlay activate`, `end` or `promote` | `action`, `name`, `for` |
| `maintenance` | changes | `muructl maintenance on` or `off` | `app`, `on` |
| `apply` | changes | `muructl apply` | `file`, `dry_run` |
| `log_level` | changes | `muructl logging level` | `target`, `levels`, `for`, `reset` |
| `access_log` | changes | `muructl logging access` | `mode`, `app`, `paths`, `for`, `reset` |
| `reopen_logs` | changes | `muructl logs reopen` | `workers` |
| `reload_config` | changes | `muructl daemon reload` | none |

Every tool is annotated for the client as reading or changing things, and
none reaches beyond the daemon. Stopping, shutting down and upgrading the
daemon, `muructl exec`, and `debug exec` and `debug attach`, are not
offered: `run_command` runs only what an application declares.
`run_command` refuses a secret input given in `inputs`, so a secret's
value never passes through the agent; give it in `secret_inputs` as an
entry of one of the daemon's [secret stores](../howto/secrets.md).
