# Developing with Muru

Muru serves this project's Python web application (WSGI or ASGI) behind
its own edge. Use it to run the application the way production runs it,
deploy each change as a release, test it, and read what Muru saw when
something fails.

Work through the `muru` MCP tools when they are available. Without them,
run `muructl` in a shell and add `--json` to any command that reports
something: it prints the same JSON the tools return, or `{"error": ...}`
with a non-zero exit status when it fails.

## What the objects are

- **Application**: a name, such as `shop`, with its routes.

- **Release**: one immutable deployment of the application's source with
  its settings, named `app@release` (`shop@r3`). A release is never
  changed; a change is a new release.

- **Traffic** goes to one release at a time, the one that serves. A
  **rollout** splits it between the serving release and a candidate.

- **Desired state** is what the daemon has been told; only its reconciler
  acts on it. Every tool records what is wanted and returns once the
  reconciler has acted or failed.

- **Overlay**: a named, temporary change to routes or settings, such as
  maintenance during a database outage.

## What each operation does to traffic

| Operation | Tool | `muructl` | Traffic |
| --- | --- | --- | --- |
| Deploy | `deploy` | `deploy APP SOURCE` | Moves to the new release once every worker is ready; the previous release drains. A release that fails to start never gets traffic |
| Deploy held | `deploy` with `hold` | `deploy APP SOURCE --hold` | Stays where it is; the new release runs beside it for testing |
| Promote | `promote` | `rollout bluegreen APP@RELEASE` | Moves to the release now |
| Roll back | `rollback` | `rollback APP` | Returns to the previous release, which is kept warm for a while after a deploy |
| Drain | `drain` | `drain APP@RELEASE` | Leaves the release; its workers stop once their requests finish |
| Canary | `rollout` with `canary` | `rollout canary APP@RELEASE --steps 10,50` | Shifts in steps; each step's gates (error rate, latency, worker faults) promote or roll back on their own |
| Apply | `apply` | `apply --file muru.daemon.toml` | Whatever the file declares: a new release in it deploys as `deploy` does |
| Apply held | `apply` with `hold` | `apply --file muru.daemon.toml --hold` | Stays where it is; every release the apply adds runs beside what serves |
| Plan a deploy | `deploy` with `dry_run` | `deploy APP SOURCE --dry-run` | None; reports the release a deploy would make, each setting with where it would come from, and what would refuse it |
| Run a command | `run_command` | `command run APP NAME --input K=V` | None; runs a command the release declares (a migration, a data fix, a check) with the release's interpreter and settings |

`deploy` adds one release; `apply` makes a declarative file the whole
desired state. Use `apply` when the project has a `muru.daemon.toml`, `deploy`
for a quick release of the working tree. `muructl render APP SOURCE
--release ID` writes a `muru.daemon.toml` for one release from a source
directory and `deploy`'s settings, without the daemon.

## The development loop

1. `status` first. If no daemon answers, start one (`muructl up`), unless
   the MCP server was started with `--up`, which has done it.

2. Deploy the working tree: `deploy` with `app` and `source` set to the
   project directory, or `apply` with the project's `muru.daemon.toml`. Wait for
   the result: it is the release, with its `state` (`ready` or `failed`)
   and a `message` saying why it failed.

3. Run the tests against the daemon's listener, `http://127.0.0.1:8080/`
   unless `status` lists other listeners.

4. When something fails, find out why before changing code:

   - `events` for what happened in order: the release's state changes,
     traffic switches, worker crashes and timeout kills.

   - `logs` with the application as `target` for its output: prints,
     logging and tracebacks. `access` gives the access log, with each
     request's status, time and the reason Muru answered when it did.

   - `debug` with `stacks` for what every worker thread is doing now and
     which request it is serving: the way to see a hung or slow request.
     `status` gives each worker's requests in flight and memory.

   - `release` for the release's settings as the daemon resolved them.

5. Fix, and deploy again. Each deploy is the next release (`r2`, `r3`, ...).
   Run the project's declared steps, such as a migration, with
   `run_command`; `commands` lists them and their inputs.

6. Clean up: `rollback` when a release made things worse, `drain` a
   release that is no longer needed.

## What to check after each operation

- After `deploy`: the release's `state` is `ready` and `serving` is
  `true` (or `false` with `hold`). On `failed`, read `message`, then
  `logs` for the traceback of the import that failed.

- After `promote` or `rollback`: `list` with `releases` shows which
  release serves.

- After a `rollout`: `list` with `rollouts` shows its step, the share of
  traffic and its state; it ends `promoted` or `rolled_back` with a
  message naming the gate.

- After `apply`: the report's `errors` are empty and `changes` lists what
  happened; with `dry_run`, what would.

- After `run_command`: `exit_code` is 0; otherwise `output` holds what the
  command printed, standard error lines marked `stderr:`.

## Things to know

- A release's errors reach the client as pages Muru generates, with a
  `reason` such as `app_error`, `worker_crashed`, `deadline` or `hard_timeout`;
  the access log records it.

- A request that runs past its timeout is ended and its worker replaced;
  `events` names it. Raise the timeout in the release's settings rather
  than retrying.

- `docs` returns pages of the user guide by name; start with the list it
  gives without a page.

- A step worth repeating (a migration, loading fixtures, a smoke check)
  can be declared as a command in the project's `[tool.muru.commands]`
  table, with its inputs; it can be run from the next release deployed.
  `docs` with `howto/commands` describes the table. Give a secret input
  in `secret_inputs` as `STORE/KEY`, never as a value.

- Do not stop, upgrade or reconfigure the daemon unless asked: other
  applications may share it.
