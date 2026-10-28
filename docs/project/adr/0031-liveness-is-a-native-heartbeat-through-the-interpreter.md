---
status: accepted
date: 2026-10-28
---

# ADR 0031: Liveness is a native heartbeat through the interpreter, in every worker

## Context and problem statement

Section 9.11 rests "worker alive" on native heartbeats and request
progress, and section 7.6 lists the failsafe layers, but neither said
what the heartbeat proves, and the implementation narrowed it to event
loops: an ASGI worker's loop beat every 500 ms, a native watchdog
replaced the worker when the loop had not run for `loop_stall_timeout`,
and a WSGI worker had no heartbeat at all. The worker's periodic report
to the host is sent by a native task, so it keeps coming while every
Python thread is wedged, and the host kept no time of the last report,
so a worker stopped whole went unnoticed too. Request progress covers a
hang with a request in flight, through the hard timeout, and nothing
covers an idle worker whose interpreter can no longer be entered: a C
call that never returns with the GIL held, or a deadlock between
threads.

The cost is not only that worker. The edge handshakes with every worker
on each snapshot apply, so one worker that cannot answer makes every
routing change on the node fail until someone finds its pid and kills
it. The question is what the primary liveness signal should be, and in
which workers.

## Decision drivers

- A worker in which no thread can run Python serves nothing, whether or
  not a request is in flight, so the signal must not depend on requests.

- The check must work in WSGI workers, where there is no event loop to
  beat.

- No false kills: a legitimately busy worker holds the GIL only in
  slices, since the interpreter switches threads every few milliseconds,
  so a heartbeat that merely needs to attach is starved only by a fault.

- A process that is stopped or frozen whole runs no native thread
  either, so the host must notice a worker that has gone silent, not
  only one that says it has stalled.

- One setting, one event path: the hard timeout's replacement path,
  with the stacks taken before the worker goes, already does what is
  wanted once a stall is known.

## Considered options

- Keep the event loop heartbeat as the only native signal and add HTTP
  liveness probes for the rest.

- A native thread in every worker that attaches to the interpreter every
  500 ms, which a held GIL stops, watched by the native watchdog that
  needs no interpreter; and the host treating a worker whose reports
  have stopped for the same period as dead.

- The worker's existing report carrying a "Python ran" flag, so the host
  alone decides.

## Decision outcome

Chosen option: the native thread in every worker, and the host timing
the reports, both under one setting, `gil_stall_timeout` (30 s). The
thread attaches to the interpreter every half second and records the
time; the watchdog reports `worker.gil_stalled` once the record is older
than the timeout, and the host takes the hard path as for the hard
timeout, with `faulthandler` writing the stacks since the interpreter
cannot answer. The host also replaces a worker that has sent no report
for the timeout. The loop heartbeat stays, with its own shorter timeout:
a loop held by synchronous code is a different fault from a held GIL,
and an ASGI worker should be replaced sooner for it.

The report flag was not chosen because the native side of a worker goes
on reporting while Python is wedged, and the host would have to infer a
stall from a flag's absence; the watchdog in the worker sees the stall
directly and the host's silence check covers the rest. HTTP probes stay
what section 9.11 makes them: opt-in, for application failures.

The default follows mod_wsgi's deadlock timeout in kind, not in length:
mod_wsgi restarts a whole daemon process group after 300 s, while here
one worker is replaced without losing capacity, so 30 s is a better
trade between a wedged worker's 503s and a false kill. On a
free-threaded build attaching never waits for another thread, so the
heartbeat there only ever stops with the process.

### Consequences

- Good: a wedged WSGI worker is found and replaced in about a minute
  with the defaults, idle or not, and no routing change on the node waits
  on it for longer.

- Good: a worker stopped by a signal, or frozen with every thread, is
  replaced too.

- Bad: a worker stopped on purpose for longer than the timeout, by a
  native debugger rather than `muructl debug attach`, is replaced;
  `attach` pins the worker, and the setting can be raised for the
  session.

- Bad: two more native threads per worker, sleeping almost all the time.

## More information

Sections 7.6 and 9.11 of the design specification; `muructl restart`,
which gives the operator the same replacement by hand for faults the
heartbeat does not see.
