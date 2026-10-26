# Inspecting workers

```{admonition} Status
:class: note

Status, stacks, tasks, inspect, logger levels, profiling, tracing,
memory, running a snippet, a pdb session with `breakpoint()`, and the
automatic captures, answered from inside each worker by the runtime's
control thread, with faulthandler when a worker cannot answer. The Debug
Adapter Protocol for IDEs, draining the debugged worker, and reading a
worker from outside with PEP 768 on Python 3.14 are still to come.
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

## Profile

`muructl debug profile` samples the stack of every thread running Python
code, every 10 ms for 10 s unless `--interval` and `--for` say otherwise,
in each worker of the release at once, and merges the samples into
collapsed stacks, the format flame graph tools read:

```
muructl debug profile shop --for 30s --output shop.folded
flamegraph.pl shop.folded > shop.svg
```

Each line is a stack, root first and under the thread's name, then how
many samples found it. The functions that took most samples are printed
on standard error with their share. Threads waiting outside Python, such
as idle threads of the pool, are not sampled, so the profile is of the
work being done. Sampling costs the worker little, so it is safe on a
release serving traffic.

## Trace

`muructl debug trace` counts every call of every Python function and the
time spent in each, with sys.monitoring, for `--for` (10 s), and prints
the functions that took longest with their call counts. It needs Python
3.12 or later. Exact counts cost the worker more than sampling, so a
worker serving requests in more than half its slots is refused unless
`--force` is given.

## Memory

`muructl debug memory` shows each worker's resident, proportional and
unique size, the garbage collector's counts, and the object types it
holds most of. With `--trace`, tracemalloc runs for `--for` (30 s) and
the sites that allocated most in that time are shown, by bytes and
objects added:

```
muructl debug memory shop --trace --for 1m --top 30
```

## Running code

`muructl debug exec` runs a snippet in the workers and prints what it
printed and the value of its last expression, or the traceback when it
raised:

```
muructl debug exec shop --worker 0 -- 'import myapp.cache; len(myapp.cache.entries)'
```

It is off unless the daemon's configuration file allows it:

```toml
[daemon]
debug_exec = true
```

Every use is the event `debug.exec` with the code that ran, as profiles,
traces and memory traces are `debug.profile`, `debug.trace` and
`debug.memory`.

## Attaching a debugger

`breakpoint()` in application code makes the worker wait there for a
debugger, for 60 seconds or the `timeout` it is given, and raises the
event `debug.breakpoint` with the worker, file and line. A WSGI request
waiting there is exempt from the soft and hard timeouts meanwhile; in an
ASGI application, `breakpoint()` holds the worker's event loop, as pdb
does, and the timeouts still apply. When nobody attaches it carries on,
with the event `debug.breakpoint_expired`.

```python
def checkout(request):
    order = build_order(request)
    breakpoint(timeout=300)
    return charge(order)
```

`muructl debug attach` opens a pdb session at the waiting breakpoint,
taking the worker that waits unless `--worker` names one:

```
$ muructl debug attach shop
muru: attached to worker 1 at /srv/shop/views.py:42; the end of input lets the code go on
> /srv/shop/views.py(43)checkout()
-> return charge(order)
(Pdb) p order.total
Decimal('120.00')
(Pdb) continue
muru: the session in worker 1 has ended
```

Each line typed is a pdb command. `continue` lets the request go on and
ends the session; so does the end of input, as does an hour in all or ten
minutes without a line. In a worker with no breakpoint waiting, the
session has a thread of its own, from which the worker's modules and
state can be examined.

A debugger runs code in the worker, so attaching needs `debug_exec =
true` as running a snippet does. Each session is the event
`debug.attached`, and `debug.detached` when it ends. Outside Muru,
`breakpoint()` is the ordinary pdb, so the call is safe to leave in
while developing; setting `PYTHONBREAKPOINT` keeps whatever it names.

## Automatic captures

Some events say what the worker was doing, captured before the worker
lost the evidence:

- `request.soft_timeout`: a request past the [soft timeout](timeouts.md),
  with where its thread was, taken before the timeout interrupted it.
- `worker.timeout_killed`: a worker replaced for the hard timeout, with
  where its oldest request was. Its threads' stacks go to the log, through
  faulthandler when the worker cannot answer.
- `worker.recycled` for memory: a worker replaced over a [memory
  limit](memory.md), with the object types it held most of.

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
