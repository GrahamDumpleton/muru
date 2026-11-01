# Running commands

A command runs with a release: the release's interpreter and environment,
the secrets its workers are given, its user and its source directory, so
a migration or a management command sees exactly what the application
sees. The release need not be serving, or running at all.

An application declares the commands it needs run, such as a migration, a
backup or creating an account, by name, with the inputs each takes, and
anyone operating it runs them with `muructl command run`: a person, a
deploy script, or a coding agent through the [MCP tools](coding-agents.md).
`muructl exec` runs anything else, once.

## Declaring commands

Commands are declared in the project's `[tool.muru]` table, in
`pyproject.toml` or `muru.toml`, and are part of each release created
from it:

```toml
[tool.muru.commands.migrate]
description = "Apply database migrations"
run = ["python", "manage.py", "migrate", "--noinput"]
timeout = "10m"
exclusive = true

[tool.muru.commands.add-user]
description = "Create an administrator"
run = ["python", "manage.py", "createsuperuser", "--noinput",
       "--username", "{username}", "--email", "{email}"]
env = { DJANGO_SUPERUSER_PASSWORD = "{password}" }
inputs.username = { required = true, pattern = "[a-z][a-z0-9_]*" }
inputs.email = { required = true, description = "Where account mail goes" }
inputs.password = { required = true, secret = true }

[tool.muru.commands.backup]
description = "Back up the uploads"
call = "shop.ops:backup"
inputs.target = { default = "s3://backups/shop" }
```

| Key | Meaning |
| --- | --- |
| `run` | The program and its arguments, run without a shell. `python` is the release's interpreter, and its directory comes first on `PATH`, so a script the environment installed is found too |
| `call` | Instead of `run`, a function as `module:function`, called with the inputs as keyword arguments |
| `description` | What the command does, shown by `muructl command list` |
| `env` | Environment variables for this command, over the release's |
| `inputs.NAME` | An input the command takes (see below) |
| `timeout` | How long it may run before it is stopped, such as `10m`; no limit when absent |
| `exclusive` | `true` to run one at a time for the application: a run asked for while one is under way is refused |

`{name}` in an argument of `run`, or in a value of `env`, is replaced by
the input's value; `{{` and `}}` are literal braces. A command's name is
lowercase letters, digits and hyphens. A [declared
release](daemon.md#declaring-applications-in-files) can carry a
`commands` table of its own, which replaces the project's.

Since commands are part of a release, a new or changed command takes
effect with the next deploy, and each release runs the commands its own
code declares.

## Inputs

| Key | Meaning |
| --- | --- |
| `required` | A run without a value is refused |
| `default` | The value when none is given |
| `pattern` | A regular expression the whole value must match |
| `secret` | The value is never shown, logged or put in an event, and may only be used in `env`, never in `run`, where the process's command line would show it |
| `description` | What the value is, shown by `muructl command list` and when asked for |

An input that is neither given nor has a default is empty in `run` and
`env`, and is left out of a callable's keyword arguments, so the
function's own default applies.

## Running a declared command

```console
$ muructl command list shop
shop@r7

migrate: Apply database migrations
  run: python manage.py migrate --noinput
  timeout 10m, one run at a time
  last run: 2026-11-01T10:02:13Z with r7, exited with status 0 after 3s

add-user: Create an administrator
  run: python manage.py createsuperuser --noinput --username {username} --email {email}
  input email (required): Where account mail goes
  input password (required, secret)
  input username (required, matching [a-z][a-z0-9_]*)
...
```

```
muructl command run shop migrate
muructl command run shop add-user --input username=amy --input email=amy@example.com
```

The second asks for the password on the terminal, without echo, since it
is required and was not given. Without a terminal a required input must
be given, and the run is refused, naming the input, before anything
starts. Two ways keep a secret off the command line:

```
printf '%s' "$PASSWORD" | muructl command run shop add-user \
    --input username=amy --input email=amy@example.com --input-stdin password

muructl command run shop add-user \
    --input username=amy --input email=amy@example.com \
    --secret-input password=local/admin-password
```

`--input-stdin` reads one input's value from standard input.
`--secret-input` names an entry in one of the daemon's [secret
stores](secrets.md) as `STORE/KEY`, which the daemon reads itself, so the
value never passes through `muructl` at all.

Name a release, `shop@r8`, to run that release's command instead of the
current release's. Output, exit status and stopping work as for
`muructl exec`, below.

## Callables

A command with `call` runs the function in a new process with the
release's interpreter, environment and source directory, so its module
imports as the application's own do:

```python
# shop/ops.py
def backup(target):
    ...
    return 0
```

The function gets the inputs as keyword arguments, and may be an `async`
function. An integer it returns is the exit status; returning anything
else is success, and an exception it raises prints its traceback and
exits with status 1. `MURU_COMMAND` names the command.

## One-off commands

`muructl exec` runs a command that is not declared, with an application's
current release:

```console
$ muructl exec shop -- python manage.py showmigrations orders
orders
 [X] 0001_initial
 [X] 0006_add_tracking
 [ ] 0007_add_carrier
```

Everything after `--` is the command and its arguments, run as `run` is,
without a shell, so quote them as you would for any program rather than
for a shell line. Name a release to run with that one instead, a held
candidate before it takes traffic say:

```
muructl exec shop@r8 -- python manage.py check --deploy
```

## Output and exit status

Output appears as the command writes it, standard error on standard
error, and `muructl` exits with the command's exit status. A command that
fails says how:

```console
$ muructl exec shop -- python manage.py migrate orders 0042
CommandError: Cannot find a migration matching '0042' from app 'orders'.
muru: the command exited with status 1 after 2s
```

The command runs in the release's source directory with the release's
environment variables; `MURU_APP`, `MURU_RELEASE` and `MURU_RELEASE_DIR`
say what it runs with. It gets no input and no terminal, so an
interactive program such as `manage.py shell` cannot be run this way.

`--json` prints each line of output, and last how the command ended, as
JSON lines (see [JSON output](../reference/json-output.md)).

## Stopping a command

Interrupting `muructl`, with Ctrl-C or by closing the session it runs in,
stops the command: it gets SIGTERM and, ten seconds later, SIGKILL, along
with anything it started. A command past its `timeout` is stopped the
same way. Run a long command where the session will last, under `nohup`,
`tmux` or a CI job, so a dropped connection does not stop it halfway.

## What is recorded

The command's output is added to the release's log, each line prefixed
with `[command NAME]` (`[command exec]` for a one-off command), so
`muructl logs shop` shows it among the workers' output. Each run is also
in the events, with its inputs, a secret's as `***`:

```console
$ muructl events
    41 10:02:13 info     command.started      shop@r7                  add-user: started (pid 52113), with email=amy@example.com, password=***, username=amy
    42 10:02:16 info     command.finished     shop@r7                  add-user: exited with status 0 after 3s
```

A command that exits with a status other than 0, or is stopped, is a
`command.failed` event. `muructl command list` shows each declared
command's last run.

## Which releases run commands

Any release of an application that runs workers: serving, held,
retained, drained or stopped. A release whose environment the daemon
builds from `uv.lock` must have started once, which builds it. A
[managed server](managed-servers.md)'s release has no interpreter of its
own and runs none.

A development run with `muructl run` has no releases to run commands
with; run them in the project's own environment instead, as
`uv run python manage.py migrate`.

## Migrations

Running migrations while two releases share a database takes some care;
[database migrations](migrations.md) covers how.
