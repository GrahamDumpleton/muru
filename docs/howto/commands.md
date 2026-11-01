# Running commands

A command runs with a release: the release's interpreter and environment,
the secrets its workers are given, its user and its source directory, so
a migration or a management command sees exactly what the application
sees. The release need not be serving, or running at all.

## One-off commands

`muructl exec` runs a command with an application's current release:

```console
$ muructl exec shop -- python manage.py migrate
Operations to perform:
  Apply all migrations: admin, auth, contenttypes, orders, sessions
Running migrations:
  Applying orders.0007_add_tracking... OK
```

Everything after `--` is the command and its arguments, run without a
shell, so quote them as you would for any program rather than for a
shell line. `python` and `python3` name the release's interpreter, and
its directory comes first on `PATH`, so a script the environment
installed, such as `flask` or `alembic`, is found too. The command runs
in the release's source directory with the release's environment
variables, and `MURU_APP`, `MURU_RELEASE` and `MURU_RELEASE_DIR` say
what it runs with.

Name a release to run with that one instead of the current release, a
held candidate before it takes traffic say:

```
muructl exec shop@r8 -- python manage.py check --deploy
```

Output appears as the command writes it, standard error on standard
error, and `muructl exec` exits with the command's exit status. A command
that fails says how:

```console
$ muructl exec shop -- python manage.py migrate orders 0042
CommandError: Cannot find a migration matching '0042' from app 'orders'.
muru: the command exited with status 1 after 2s
```

A command gets no input and no terminal, so an interactive one such as
`manage.py shell` or `createsuperuser` without `--noinput` cannot be run
this way.

## Stopping a command

Interrupting `muructl exec`, with Ctrl-C or by closing the session it
runs in, stops the command: it gets SIGTERM and, ten seconds later,
SIGKILL, along with anything it started. Run a long command where the
session will last, under `nohup`, `tmux` or a CI job, so a dropped
connection does not stop it halfway.

## What is recorded

The command's output is added to the release's log, each line prefixed
with `[command exec]`, so `muructl logs shop` shows it among the
workers' output. Each run is also in the events:

```console
$ muructl events
    41 10:02:13 info     command.started      shop@r7                  exec: started (pid 52113)
    42 10:02:16 info     command.finished     shop@r7                  exec: exited with status 0 after 3s
```

A command that exits with a status other than 0, or is stopped, is a
`command.failed` event.

`--json` prints each line of output, and last how the command ended, as
JSON lines (see [JSON output](../reference/json-output.md)).

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
