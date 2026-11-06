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

With no application named, the same commands run for [the project in the
current directory](#the-projects-own-commands), with no daemon: in
development, in a container, or in a Kubernetes Job.

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

When `muructl` runs the command itself, because nothing serves the
project, Ctrl-C or SIGTERM stops the command the same way. `muructl`
waits for it to end, says why it was stopped, and exits with 128 plus the
signal it got, 130 for Ctrl-C and 143 for SIGTERM:

```console
$ muructl command run migrate
Running migrations:
^C
muru: the command stopped because muructl got SIGINT; it was killed by signal 15 after 4s
```

A second Ctrl-C or SIGTERM kills the command, and anything it started,
at once.

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

A development run with `muructl run`, and `muructl serve`, have no
releases to name. Their commands are [the project's
own](#the-projects-own-commands).

## The project's own commands

Leave the application out, and a command is one of the project in the
current directory:

```
muructl command list
muructl command run migrate
muructl command run add-user --input username=amy --input-stdin password
muructl exec -- python manage.py showmigrations orders
```

The commands are the ones the project's file declares now. Each runs in
the project directory with the interpreter, the runtime library and the
`[tool.muru.env]` variables the application is served with, over the
environment `muructl` itself has. Inputs, callables, `timeout`, output,
the exit status and stopping work as they do for a release's command.

Where the command runs depends on what is running:

- **Through `muructl run` or `muructl serve`.** When a development run
  of the project is up, or `muructl serve` is running in the container,
  the command runs through it. Its output joins that process's output,
  prefixed `[command NAME]`, so the terminal, the container's log and
  `muructl logs` show it, and its start and end are printed as
  `command.started`, `command.finished` and `command.failed` lines.
  `MURU_RELEASE` names the release that serves, `r1` and so on.
- **By `muructl` itself.** When nothing serves the project, `muructl`
  starts the command and waits for it. Nothing else needs to be running,
  which is what a Kubernetes Job, an init container, a Helm hook or a
  step of a CI job needs. Its output goes to standard output and
  standard error alone, and `MURU_RELEASE` is not set.

`--chdir DIR` names the project from another directory, as it does for
`muructl run`. A command `muructl` runs itself finds the rest as
`muructl serve` and `muructl run` do:

| What | Where it is looked for, in order |
| --- | --- |
| The project | `--chdir`, `MURU_SERVE_CHDIR`, the current directory |
| The project's settings | The file `--config` or `MURU_SERVE_CONFIG` names, as [`muructl serve` takes it](containers.md#a-settings-file-from-outside-the-image); else the `muru.toml` or `pyproject.toml` in the project |
| The interpreter | `--python`, `MURU_SERVE_PYTHON`, `python` in the settings' serve table, `python` in the settings, the active virtual environment, the project's `.venv`, `/app/.venv`, `python3` on `PATH` |
| The runtime library | The interpreter's environment when it holds this version of `muru`; else the directory `--runtime-dir`, `MURU_SERVE_RUNTIME_DIR` or the serve table's `runtime_dir` names; else `/opt/muru` when it holds one; else where `muructl run` keeps it |

A Job and a Deployment made from one image, with the same environment
variables, so find the same settings, interpreter and library. No environment is
built for a command: build it first, with `muructl build` or by running
the application once.

What differs by where a command runs:

| | A named application, under the daemon | Through `muructl run` or `muructl serve` | By `muructl` itself |
| --- | --- | --- | --- |
| The code | The release's source | The project directory | The project directory |
| `exclusive = true` | One run at a time for the application | One run at a time for that process | Not checked: one run at a time is up to what starts it, such as a Job with `parallelism: 1`, a Helm hook or the migration tool's own lock |
| `--secret-input` | Read from the daemon's secret stores | Refused; give the value with `--input-stdin` | Refused; give the value with `--input-stdin` |
| Output and events | The release's log and the events | That process's output | Standard output and standard error |
| The last run in `muructl command list` | Kept | Not kept | Not kept |

A project whose `[tool.muru.env]` takes a value from a secret store is
served by the daemon alone, and its commands are run there too. The
secrets a container is given are in its environment or mounted as files,
and a command sees them as the application does.

With an application named the command always goes to the daemon, which
runs it with that release's own code and not with what is in the current
directory. `--chdir`, `--config`, `--python` and `--runtime-dir` are
refused there.

## Migrations

Running migrations while two releases share a database takes some care;
[database migrations](migrations.md) covers how.
