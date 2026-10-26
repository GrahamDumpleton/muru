---
status: accepted
date: 2026-10-26
---

# ADR 0028: Workers are read from outside on Linux, and asked on macOS

## Context and problem statement

Section 11.7 builds `muructl debug stacks`, `tasks` and `profile` on PEP
768: the host reads its workers' interpreters from outside, with no
cooperation from them, so a worker whose threads hold the interpreter or
spin can still be seen. It says the host has the rights this needs
because it is the workers' parent and runs as their user, "the same user
suffices on macOS and on Linux with the default ptrace scope".

Linux bears that out: Yama's default scope lets a process read its
descendants. macOS does not. Reading another process needs its task port,
which macOS gives only to root or to a binary with the
`com.apple.system-task-ports` entitlement, whoever its parent is. Hosts
run as the application's user (ADR 0026), never root, and a Python
interpreter carries no such entitlement. The question is what the host
does on macOS, and how much of section 11.7 moves outside.

## Decision drivers

- Nothing but the daemon runs as root (ADR 0026), and no part of Muru
  should ask for an entitlement or a code signature to work.

- A hung worker should still be seen wherever that is possible, which is
  what reading from outside is for.

- Every command should keep working everywhere, from inside where it
  cannot be done from outside.

## Considered options

- Read from outside where the host can, Linux in practice, and ask the
  worker itself elsewhere, as on interpreters before 3.14.

- Have the daemon, which may be root, do the reading on macOS.

- Leave the reading from outside for later everywhere.

## Decision outcome

Chosen option: read from outside where the host can. The host tries the
first read, and when the operating system refuses it, logs once and asks
its workers from then on. The daemon is not made a reader: it would need
root for a development convenience, and macOS is a development platform
for Muru, not a production one.

What is read from outside is the stacks, for `debug stacks` and for the
capture before a hard-timeout kill, and the samples of `debug profile`
over everything a worker runs. Three things stay inside:

- `debug tasks`, which also measures the event loop's lag, something only
  the loop can do;

- `debug profile` with `--path` or `--requests`, which choose threads by
  the requests they serve at each sample;

- the soft timeout's capture, which the worker makes of its own thread
  before it interrupts it.

Threads read from outside carry no names, and the request each serves is
the runtime's bookkeeping. The runtime answers a `bookkeeping` request on
its control thread without the interpreter, from its own records, so a
worker whose threads hold the interpreter still says which request each
thread serves and what the runtime's threads are called.

### Consequences

- Good: on Linux a worker stuck in C code with the interpreter held, or
  stopped, shows its stacks at once rather than after the in-process
  timeout and a faulthandler dump to the log.

- Good: sampling from outside costs a worker nothing, as section 11.7
  intends.

- Bad: on macOS, outside reading is unavailable unless Muru runs as root,
  so those commands behave as on Python 3.13 there. The integration tests
  that need it say so and are counted as unavailable on macOS.

- Neutral: the reader cannot start from a thread whose only frame is its
  interpreter's base frame, which is where a thread the runtime made from
  Rust waits for work, and fails for the whole worker when one does. Each
  application thread therefore waits inside a Python function of the
  runtime's, `muru._pool.serve_jobs`, which hands the thread back to the
  runtime for its whole life, and the readers leave threads waiting there
  out.

- Bad: a frame read from outside is labelled by the line it was running,
  where the in-process profile labels a function by its first line, so a
  profile from outside splits a function by its lines.

## More information

Section 11.7 of the design specification; PEP 768; the `_outside` module
of the runtime library and the host's `stacks_from_outside` and
`profile_from_outside`; ADR 0025 on the in-process mechanism and ADR 0026
on who runs as root.
