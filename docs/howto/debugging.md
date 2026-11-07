# Inspecting workers

`muructl debug` looks into the workers of a release through the daemon,
the host and the worker's control connection, so nothing listens anywhere
new. Each command takes a release as `app@release`, or an application for
its current release, and asks every worker unless `--worker N` names one;
`--json` prints each worker's answer as JSON.

A development run serves the same commands: `muructl run` has an admin
socket of its own, which a command run from the project directory finds
without a flag, and the run's application and release are the ones its
terminal names, so `muructl debug stacks app` reads the run's workers.
The application can be left out, since a run serves only one:
`muructl debug stacks` does the same, and so does `muructl serve`'s
socket inside a container.
`debug exec` and `debug attach` need no `debug_exec` there, since the
person at the terminal started the workers. `muructl status` says which
socket a command found.

```
muructl debug status shop
muructl debug stacks shop@r3 --worker 1
muructl debug tasks api
muructl debug inspect shop --json
```

The daemon's [dashboard](dashboard.md#looking-into-the-workers) shows
the status, stacks, tasks and memory of a release's workers too, and so
does a run's, from the Workers link on its first page.

## Status

Each worker's pid, group, uptime, requests served, what it holds as of
the moment it answers, its memory, whether the edge routes to it, and
its state.

```
WORKER  PID      GROUP      UP       REQUESTS   THREADS  THREAD-OLDEST  RSS     USS     ROUTED  STATE
0       41213    default    5m18s    12044      1/4      0.1s           48MB    31MB    yes     ready
1       41214    default    5m18s    11980      0/4      -              47MB    30MB    yes     ready
```

THREADS is the application threads busy of the worker's threads, and
THREAD-OLDEST how long the longest has been held: a request holds its
thread from the moment a pool thread picks it up, so one waiting at
`breakpoint()` or streaming its response, which the timeouts leave
alone, is counted for as long as it holds the thread. A worker with an
event loop shows LOOP, the requests on the loop of its `concurrency`,
and LOOP-OLDEST; open WebSockets are left out of both, since a socket
may stay for hours, and `muructl websockets` counts them. Beside them,
SYNC is the threads running synchronous code for the loop, of the
threads that may: an ASGI application whose views call blocking code
fills them, and at `8/8` every new call waits for a thread while the
loop itself has room. Such code runs on one of
[three pools](asgi.md#one-event-loop-per-worker), the loop's own,
anyio's and asgiref's shared thread, and SYNC is the fullest of them,
the one whose calls wait first; `--json` gives each as `sync_pools`.
SYNC-OLDEST is the age of the oldest request with a call on the loop's
pool or asgiref's thread, and a dash when the busy threads are anyio's.

```
WORKER  PID      GROUP      UP       REQUESTS   LOOP      LOOP-OLDEST  SYNC    SYNC-OLDEST  RSS     USS     ROUTED  STATE
0       41213    default    5m18s    12044      31/1000   4.2s         8/8     4.2s         52MB    34MB    yes     ready
```

A worker shows the sides it has, and the table the columns any worker
of the release has, so a WSGI release has the thread columns and an
ASGI release the loop and sync columns. `muructl debug stacks` names,
on each thread of the pool, the request whose call it is running. Every header is one word, so a script can
pick a column by its name.

The figures are as of the moment the status answers: the host asks each
worker, and one that does not answer within a second, which is a hung
one, is shown from its last report, up to a second old, its figures
marked with a star. STATE is `starting`, `ready`, `debugging` (a
debugger session is open, so the worker takes no requests), `replacing`
(a fresh worker is taking its place) or `retiring` (scaled in). ROUTED
says whether the edge sends it requests: a replacement that is ready
but not yet announced to the edge reads `ready` and `no`.

## Requests in flight

`muructl requests` shows the requests in flight now, as `muructl
websockets` shows open sockets: those queued at the edge for a worker
slot, those holding a thread, and those on an event loop, with the age
of the oldest of each, one row per worker group, since the edge queues
per group and a count per release would hide which group is starved.
The thread columns show for a release whose workers run requests on
threads, the loop columns for one with an event loop:

```console
$ muructl requests shop
RELEASE              GROUP        QUEUED  QUEUED-OLDEST  THREADS  THREAD-OLDEST  LOOP   LOOP-OLDEST
shop@r7              default      3       1.2s           8/8      1m02s          14     31.0s
shop@r7              reports      0       -              2/4      4.1s           -      -
```

`--list` lists them, the oldest first, with `--limit` (100) on how many:
the age, whether the request is queued or running, its kind, the worker
that has it, the method and target, the client and the request id.

```console
$ muructl requests shop --list --limit 5
AGE      STATE    KIND            WORKER  REQUEST                                  CLIENT           REQUEST-ID
1m02s    running  wsgi            1       GET /checkout                            10.0.0.7         01M5R1GN3RD59QX94CBQH28ENT
31.0s    running  asgi            0       GET /live/summary                        10.0.0.9         01M5R1GN3RD59QX94CBQH28ENV
4.1s     running  wsgi            1       GET /reports/annual                      10.0.0.7         01M5R1GN3RD59QX94CBQH28ENW
1.2s     queued   -               -       GET /reports/monthly                     10.0.0.12        01M5R1GN3RD59QX94CBQH28ENX
0.8s     running  wsgi streaming  0       GET /events                              10.0.0.3         01M5R1GN3RD59QX94CBQH28ENY
```

A request's kind is what the edge can tell of it as of now: `wsgi` or
`asgi` by the side of the worker it was sent to, `wsgi streaming` or
`asgi streaming` once its response head asked for immediate flushing
(`text/event-stream`, or `X-Accel-Buffering: no`), which the timeouts
leave alone but which holds its thread or loop slot, `grpc`, `handoff`
once the worker handed it to its other interface, and `upstream` for a
request sent to a managed server. A request that became a WebSocket
leaves the list and is counted by `muructl websockets`; a static file
is answered by the edge at once and is never listed.

THREADS and LOOP here are what the edge charged each request to, from
what the worker declared when it connected. `debug status` shows what
the worker itself holds, so the two can differ for a worker serving
both WSGI and ASGI. What a worker does inside a request, such as a
thread waiting at `breakpoint()`, stays with `debug stacks` and
`debug status`. The command works against the daemon, a development run
and `muructl serve`, and `--json` gives the same figures.

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

On Linux with Python 3.15 or later the host reads the stacks from
outside the worker (PEP 768), so a worker stuck in C code with the
interpreter held, or stopped altogether, shows them at once. Elsewhere,
on older interpreters and on macOS, where reading another process needs
root, the worker is asked. A worker asked that cannot answer in five
seconds, because something holds the interpreter, is reported as such,
and the host sends it SIGQUIT so that `faulthandler`, which needs no
interpreter, writes every thread's stack to the release's log instead;
`muructl logs APP` shows it. On Linux with Python 3.14 such a worker is
also read from outside, which there gives only each thread's innermost
frames, up to the first call from C code; the log has the whole stacks.

## Tasks

For ASGI workers, the tasks on the event loop, the coroutine each runs,
what it awaits and where it is, and the loop's lag: how long a callback
scheduled from another thread waited to run.

## Inspect

Each worker's interpreter and executable, working directory, `sys.path`,
the modules loaded with versions where they declare one, threads, open
descriptors and environment. The value of a key that looks like a
secret (`SECRET`, `TOKEN`, `PASSWORD`, `KEY`, `CREDENTIAL` or `AUTH` in
its name, or a name ending in `_DSN`) is replaced by `***`, and a
password in a URL under any other key is: `DATABASE_URL` holding
`postgres://shop:hunter2@db/shop` is shown as
`postgres://shop:***@db/shop`.

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
many samples found it. A function is named with the file and the line it
starts on, so its samples are counted together whichever of its lines
was running. The functions that took most samples are printed
on standard error with their share. Threads waiting outside Python, such
as idle threads of the pool, are not sampled, so the profile is of the
work being done.

A profile at the default interval makes no measurable difference to a
busy worker's throughput or latency, so it is safe on a release serving
traffic. On Linux with Python 3.15 or later the samples are read from
outside the worker, which is not interrupted at all. Elsewhere, and
with `--path` or `--requests`, a thread of the worker takes them: with
the GIL it waits its turn for the interpreter like any other thread,
and on a free-threaded build it pauses the other threads for the moment
each sample takes. Sampling much more often costs more in proportion;
at a 1 ms interval, expect a busy worker to lose a few percent.

On a busy worker, `--path` samples only the threads serving requests
whose path starts with a prefix, and `--requests` ends the profile once
that many of them have finished, within `--for`:

```console
muructl debug profile shop --path /reports --requests 20 --for 5m --output reports.folded
```

A request shorter than the interval may finish unseen and is not
counted. Requests on an ASGI worker's event loop have no thread of their
own and are not sampled this way.

`--format pstats --output FILE` writes the samples as Python's pstats
instead, for `python -m pstats`, snakeviz and other tools that read
it. The figures come from the samples: a function's calls are the
samples it appeared in, its own time the samples it was running in, and
its cumulative time the samples it was on the stack for, each sample
worth one interval.

## Trace

`muructl debug trace` counts every call of every Python function and the
time spent in each, with sys.monitoring, for `--for` (10 s), and prints
the functions that took longest with their call counts. It needs Python
3.12 or later. Exact counts cost the worker more than sampling, so a
worker serving requests on more than half its threads, or holding more
than half its event loop's concurrency, is refused unless `--force` is
given. The worker keeps serving while the trace runs, so
the trace counts the requests it takes meanwhile, each slowed by the
tracing.

## Memory

`muructl debug memory` shows each worker's resident, proportional and
unique size, the garbage collector's counts, the collections it has run
and the object types it holds most of. With `--trace`, tracemalloc runs
for `--for` (30 s) and
the sites that allocated most in that time are shown, by bytes and
objects added:

```
muructl debug memory shop --trace --for 1m --top 30
```

To compare two moments of your choosing, such as before and after a
batch job, `--snapshot` keeps a tracemalloc snapshot in each worker,
starting tracemalloc if it is off, and `--diff` later shows the sites
that allocated most since then. The snapshot is kept for further
`--diff`s until `--drop`, which also stops tracemalloc if `--snapshot`
started it:

```
muructl debug memory shop --snapshot
muructl debug memory shop --diff
muructl debug memory shop --drop
```

The collections are timed in every worker from the moment it starts, by
generation (`0`, `1` and `2`, or `full` on a free-threaded interpreter,
whose collector takes the whole heap each time): how many ran, how long
they took in all and at the 95th percentile, how many objects they freed,
and how many of them ran while the worker had requests in flight, with
how many requests were waiting through them:

```
worker 0 (pid 4133)
  rss 61.2MB  pss 38.9MB  uss 30.1MB
  gc counts [412,3,0]
  gc generation 0: 1840 collections, 92.3 ms in all, p95 under 0.1 ms, 12 objects freed, 1611 while 2904 requests were in flight
  gc generation 2: 3 collections, 48.0 ms in all, p95 under 50 ms, 219 objects freed, 2 while 5 requests were in flight
  objects:
```

A collection pauses the whole worker, so pauses that landed on requests
in flight are tail latency no single endpoint explains. The same figures
are in the [metrics](metrics.md) as `muru_gc_pause_seconds`, and the
[scaling](scaling.md) diagnosis names collection when it takes a tenth of
the workers' time or more. The host already calls `gc.freeze()` before
every fork, so a worker's collections scan only what the worker itself
created; an application whose collections still cost it can create fewer
long-lived cyclic objects after the fork, or call `gc.freeze()` itself once
its caches are warm.

## Running code

`muructl debug exec` runs a snippet in the workers and prints what it
printed and the value of its last expression, or the traceback when it
raised:

```
muructl debug exec shop --worker 0 -- 'import myapp.cache; len(myapp.cache.entries)'
```

It is off unless the daemon's configuration file allows it, and on
under `muructl run`, where the person at the terminal started the
workers:

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
taking the worker that waits unless `--worker` names one. The event's
line names the command to run, with the application as the daemon or
the run calls it, and under `muructl run` it works as it is, with no
`debug_exec` to set:

```
$ muructl debug attach shop
muru: attached to worker 1 at /srv/shop/views.py:42; the end of input lets the code go on
> /srv/shop/views.py(42)checkout()
-> breakpoint(timeout=300)
(Pdb) p order.total
Decimal('120.00')
(Pdb) continue
muru: the session in worker 1 has ended
```

The session stops on the `breakpoint()` line itself, the line the
attach line names, as pdb does on Python 3.13 and later; on 3.11 and
3.12 it stops at the line after it. Each line typed is a pdb command.
`continue` lets the request go on and ends the session; so does the end
of input, as does an hour in all or ten minutes without a line. In a
worker with no breakpoint waiting, the session has a thread of its own,
from which the worker's modules and state can be examined.

While a session is open its worker takes no new requests, and nothing
replaces it: recycling, the hard timeout, the memory limits and scaling
in all wait until the session ends, when the worker serves again.
`muructl debug status` shows it as `debugging` meanwhile.

### From an IDE

`--dap` carries the Debug Adapter Protocol instead of a pdb session, so
an IDE such as VS Code debugs the worker through muructl. The release's
environment needs debugpy installed. muructl listens for the IDE on
`127.0.0.1:5678`, or the address `--listen` gives, and attaches once it
connects:

```
$ muructl debug attach shop --dap
muru: waiting for an IDE to attach to 127.0.0.1:5678 with the Debug Adapter Protocol
muru: attached to worker 1 at /srv/shop/views.py:42; the IDE disconnecting lets the code go on
```

In VS Code that is an `attach` configuration naming the address:

```json
{
  "name": "Muru worker",
  "type": "debugpy",
  "request": "attach",
  "connect": { "host": "127.0.0.1", "port": 5678 },
  "pathMappings": [{ "localRoot": "${workspaceFolder}", "remoteRoot": "/srv/shop" }]
}
```

A request waiting at `breakpoint()` stops at the line after it once the
IDE has attached, unlike pdb, and the IDE can step through it from
there; with none waiting, the IDE sees the worker's threads and can
pause them. As with
pdb, the worker takes no new requests meanwhile. Disconnecting ends the
session and lets the code go on.

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
- `worker.timeout_killed`: a worker replaced for the hard timeout, after
  `worker.loop_stalled` or `worker.gil_stalled`, or because its reports
  stopped, with where its oldest request was. Its threads' stacks go to
  the log, through faulthandler when the worker cannot answer.
- `worker.restarted` with `--force`: a worker [killed on
  request](recycling.md#restarting-a-worker-by-hand), its stacks taken
  the same way first.
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
the same commands work on a release running as another user, and on
an application in a container under `muructl serve`.
