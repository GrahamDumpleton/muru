# JSON output

Every `muructl` command that reports something takes `--json` and prints
what it reports as JSON instead of text, for scripts, CI jobs and coding
agents.

## The shape

- **The admin API's answer, with its field names.** A command that asks the
  daemon (or a development run) for something prints the admin API's answer
  for it as one indented JSON object. Field names are those of the
  `muru.admin.v1` protocol, in snake case: `current_release`,
  `resource_version`, `time_unix_ms`. Numbers are JSON numbers, times are
  milliseconds since the epoch in fields ending `_unix_ms`, and nested
  records are objects. A field the answer does not set is present with its
  empty value (`""`, `0`, `false`, `[]`), and an absent nested record is
  `null`.

- **A list is wrapped in its answer.** `muructl app list --json` prints
  `{"apps": [...]}`, not a bare array, so a field can be added to an answer
  without changing how a script reads it.

- **Streams are JSON lines.** `muructl events --follow --json` and
  `muructl logs --json` print one compact JSON object per line as each
  arrives.

- **Errors are JSON too.** A command given `--json` that fails prints
  `{"error": "..."}` on standard output, with the sentence it would have
  printed, and exits with the same status as without `--json`.

```console
$ muructl release show shop@r3 --json
{
  "app": "shop",
  "id": "r3",
  "state": "ready",
  "message": "",
  "source": "/srv/shop",
  "entrypoint": "shop.wsgi:application",
  ...
  "serving": true,
  ...
}
$ muructl release show shop@r9 --json
{
  "error": "release shop@r9 does not exist"
}
```

## Commands

| Command | Prints |
| --- | --- |
| `muructl status` | The daemon's or run's status: `version`, `pid`, `listeners`, `app_count`, `release_count`, `edge_pid` and the rest |
| `muructl daemon status` | The same as `muructl status` |
| `muructl daemon reload` | What was reloaded: `message` |
| `muructl app list` | `{"apps": [...]}`, each with `name`, `current_release`, `previous_release`, `user`, `maintenance` and the rest |
| `muructl release list` | `{"releases": [...]}`, each a release as `release show` prints it |
| `muructl release show` | The release: `app`, `id`, `state`, `message`, `source`, `entrypoint`, `serving`, its settings, mounts and groups; for a managed server, `server_instances`, each with `index`, `pid`, `address` and `started_unix_ms` |
| `muructl release wait` | The release once it reached a state waited for |
| `muructl host list` | `{"hosts": [...]}` |
| `muructl route list` | `{"routes": [...]}` |
| `muructl upstream list` | `{"upstreams": [...]}` |
| `muructl deploy` | The new release, as `release show` prints it; with `--no-wait`, as recorded |
| `muructl rollback` | The release traffic went back to |
| `muructl drain` | The release drained |
| `muructl rollout canary` | The rollout as it started, or with `--wait` as it finished: `id`, `app`, `candidate`, `baseline`, `state`, `step`, `percent`, `message`, its `steps` and its gates |
| `muructl rollout bluegreen` | The release that now serves |
| `muructl rollout status` | `{"rollouts": [...]}`, or with `--wait` the rollout as it finished |
| `muructl rollout pause` | The rollout after the change |
| `muructl rollout resume` | The rollout after the change |
| `muructl rollout promote` | The rollout after the change |
| `muructl rollout abort` | The rollout after the change |
| `muructl scale` | Each worker group's scaling state after the change: `app`, `release`, `groups` |
| `muructl scale status` | Each worker group's policy, workers, signals, last decision and churn: `app`, `release`, `groups` |
| `muructl restart` | `{"release": ..., "workers": [...]}`, each with `index`, `pid`, `new_pid`, `forced`, and `served_by` once the new worker serves (`null` with `--no-wait`) |
| `muructl overlay list` | `{"overlays": [...]}`, each with `name`, `active`, `state`, `trigger`, `changes` and its times |
| `muructl overlay preview` | `{"changes": [...]}`: what activating the overlay now would change |
| `muructl overlay activate` | The outcome: `message` |
| `muructl overlay end` | The outcome: `message` |
| `muructl overlay promote` | The outcome: `message` |
| `muructl maintenance on` | `app`, `on` and `since_unix_ms` |
| `muructl maintenance off` | `app`, `on` and `since_unix_ms` (0) |
| `muructl dashboard login` | The sign-in link: `url`, `code`, `expires_in_s`, and with `--token` the `token` |
| `muructl apply` | The report: `errors`, `warnings`, `notes`, `changes`, `unchanged`, `applied`. Each change has `action`, `subject`, `detail` and, once applied, `outcome`, which for a release that failed is `failed:` and the reason |
| `muructl config diff` | The report, as `apply` prints it |
| `muructl config export` | The desired state as a JSON document rather than TOML |
| `muructl config show` | `document` (TOML), with `--effective` also `attributions` and `layers`, and where it came from: `version`, `node`, `time_unix_ms`, `state_version` |
| `muructl mcp skill status` | How the project's copy of the skill compares: `path`, `state` (`current`, `missing` or `out_of_date`), `version` and `installed_version` |
| `muructl secret list` | `{"store": ..., "keys": [...]}`; values are never shown |
| `muructl events` | `{"events": [...]}`, each with `sequence`, `time_unix_ms`, `type`, `subject`, `level`, `message`; with `--follow`, one event per line |
| `muructl logs` | One line per log line: a release's output as `{"app", "release", "line"}`, the access log (`--access`) as the records it keeps |
| `muructl command list` | `app`, `release` and `commands`, each with `name`, `description`, `run` or `call`, `inputs` (`name`, `description`, `required`, `default`, `pattern`, `secret`), `timeout`, `exclusive` and its `last_run` (`release`, `started_unix_ms`, `duration_ms`, `exit_code`, `ok`, `message`) |
| `muructl command run` | As `muructl exec` prints it |
| `muructl exec` | One object per line of output, with `release`, `line` and `stderr`, then one with `finished` true, the `exit_code` and the ending as a `message` |
| `muructl logs reopen` | What reopened its files: `edge`, `hosts` |
| `muructl metrics` | `{"samples": [...]}`, each with `name`, `type` (as its family declares, else `untyped`), `labels` and `value`; a value JSON cannot hold (`NaN`, `+Inf`) is kept as text |
| `muructl websockets` | `open`, `endpoints` by application and pattern, and with `--list` each open `sockets` entry |
| `muructl requests` | `in_flight`, `groups` (one per worker group: `app`, `release`, `group`, `queued` and `queued_oldest_ms`, `threads` (whether the group's workers run requests on threads), `threads_busy`, `threads_of` and `threads_oldest_ms`, `event_loop`, `event_loop_busy` and `event_loop_oldest_ms`), and with `--list` each `requests` entry: `request_id`, `client`, `app`, `release`, `group`, `method`, `target`, `state` (`queued` or `running`), `kind`, `worker` (-1 with none), `started_unix_ms` and `age_ms` |
| `muructl logging level` | One debug answer for each logger set, as an array |
| `muructl logging access` | `overrides`, the changes in force after the command, the newest last, each with `mode`, `app`, `paths` and `until_unix_ms`, and `reset`, how many `--reset` put back |
| `muructl debug status` | The debug answer: `{"release": ..., "workers": [...]}`, each worker with `index`, `pid`, `ok`, `error` and its `result`: `group`, `generation`, `socket`, `state` (`starting`, `ready`, `debugging`, `replacing` or `retiring`), `ready`, `routed`, `debugging`, `uptime_s`, `requests`, `threads` and `loop` (each `busy`, `of` and `oldest_ms`, present for the sides the worker has), `stale` (the sides are from the worker's last report, since it did not answer within a second) and its memory figures |
| `muructl debug stacks` | The debug answer |
| `muructl debug tasks` | The debug answer |
| `muructl debug inspect` | The debug answer |
| `muructl debug memory` | The debug answer |
| `muructl debug profile` | The debug answer, with the samples rather than a written profile |
| `muructl debug trace` | The debug answer |
| `muructl debug exec` | The debug answer, with each worker's output and value |
| `muructl debug attach` | Takes the flag with the other debug commands; the session itself is pdb's or the IDE's |
