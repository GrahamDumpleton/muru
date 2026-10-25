# Inspecting workers

```{admonition} Status
:class: note

Status, stacks, tasks, inspect and logger levels, answered from inside
each worker by the runtime's control thread, with faulthandler when a
worker cannot answer. Profiling, tracing, memory tracing, attaching a
debugger, running a snippet and the automatic captures are still to come,
as is reading a worker from outside with PEP 768 on Python 3.14.
```

`muructl debug` looks into the workers of a release through the daemon,
the host and the worker's control connection, so nothing listens anywhere
new. Each command takes a release as `app@release`, or an application for
its current release, and asks every worker unless `--worker N` names one;
`--json` prints each worker's answer as JSON.

```
muructl debug status shop
muructl debug stacks shop@r3 --worker 1
muructl debug tasks api
muructl debug inspect shop --json
```

## Status

What the host knows about each worker from its reports: pid, group,
uptime, requests served and in flight, the age of its oldest request, its
memory, and whether it is ready or being replaced.

```
WORKER  PID      GROUP      UP        REQUESTS   IN-FLIGHT  OLDEST    RSS       USS       STATE
0       41213    default    318s      12044      1          12ms      48MB      31MB      ready
1       41214    default    318s      11980      0          0ms       47MB      30MB      ready
```

## Stacks

The Python stack of every thread in every worker, with the request a
thread is serving, taken from the runtime's own bookkeeping: its id,
method, path and how long it has run. Threads that show the same stack
across workers are shown once with the workers they came from, so
sixteen workers waiting on the same lock read as one stack.

```
muru-app-0 in worker 1:
  serving GET /reports/monthly for 42.3 s (request 01M5R1GN3RD59QX94CBQH28ENT)
  File "/srv/shop/shop/reports.py", line 88, in monthly
    rows = cursor.fetchall()
```

A worker that cannot answer in five seconds, because something holds the
interpreter, is reported as such, and the host sends it SIGQUIT so that
`faulthandler`, which needs no interpreter, writes every thread's stack
to the release's log instead; `muructl logs APP` shows it.

## Tasks

For ASGI workers, the tasks on the event loop, the coroutine each runs,
what it awaits and where it is, and the loop's lag: how long a callback
scheduled from another thread waited to run.

## Inspect

Each worker's interpreter and executable, working directory, `sys.path`,
the modules loaded with versions where they declare one, threads, open
descriptors and environment, with values of keys that look like secrets
(`SECRET`, `TOKEN`, `PASSWORD`, `KEY`, `CREDENTIAL`, `AUTH`) replaced by
`***`.

## Logger levels

`muructl logging level` sets a logger's level in every worker of a
release, for a period or until reset, without a restart:

```
muructl logging level shop shop.payments=DEBUG --for 10m
muructl logging level shop =DEBUG --for 1m        # the root logger
muructl logging level shop --reset
```

Each worker reports the level it set and the one it replaced. Levels
changed this way revert when the period ends, on `--reset`, and when the
worker is replaced, since a new worker starts from the release's
[logging defaults](logging.md).

## How it reaches the worker

Every worker has a control connection to its host, created at fork, and
the host has one to the daemon. A request travels daemon to host to
worker, where a thread of the runtime library takes it, attaches to the
interpreter only to answer, and sends the answer back the same way. The
daemon never reaches into a worker process directly, which is what lets
the same commands work on a release running as another user or, later,
in a container.
