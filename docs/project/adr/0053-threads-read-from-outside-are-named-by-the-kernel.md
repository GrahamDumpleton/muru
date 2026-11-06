---
status: accepted
date: 2026-11-07
---

# ADR 0053: Threads read from outside are named by the kernel on Linux

## Context and problem statement

On Linux the host reads its workers' stacks from outside (section 11.7,
ADR 0028), where PEP 768's reader gives each thread its TID and frames
but no name. ADR 0028 left the names to the runtime's bookkeeping, which
names only the threads that tell the runtime theirs, so every other
thread Python starts showed as `thread-N`: the worker's own event loop
thread among them (issue 405), and every thread an application starts.
The question is where the names of those threads come from, given that
reading from outside exists for a worker whose threads may hold the
interpreter, and that threads start and end between any two reads.

## Decision drivers

- A worker whose threads hold the interpreter should be named as fully
  as a healthy one, so the names must not need the interpreter.

- The name and the stack must belong to the same thread, which means
  joining by an identifier both readers share, read as close together
  as possible.

- Muru should not patch the standard library's private methods, or take
  the profile or trace hook that profilers and debuggers use.

## Considered options

- Read the name the kernel holds for each TID from
  `/proc/<pid>/task/<tid>/comm`, which `threading` sets on Python 3.14
  and later, and have Muru's own Python threads also tell the runtime
  their names.

- Have every `threading.Thread` tell the runtime its name, by wrapping
  `Thread._set_os_name`, the private method Python calls as a thread
  starts and when it renames itself.

- Ask the worker for its `threading` table, keyed by native identifier,
  beside the bookkeeping, and join the two by TID.

- Name only Muru's own threads.

## Decision outcome

Chosen option: read the kernel's name, and have Muru's own threads tell
the runtime theirs. Since Python 3.14 `threading` gives the kernel each
thread's name, in the thread itself, before its target runs, and again
when the thread renames itself, which covers every interpreter the host
reads from outside. The reader's thread identifier on Linux is the TID,
so the host reads the name by the same key, from its own side, as soon as
it has read the frames, with nothing asked of the worker. A thread that
ends in between leaves no file and is shown as `thread-N`; a TID is used
again only once the kernel's identifiers wrap, so a wrong name from reuse
is not a practical risk.

The runtime's names come first, since they are whole. The event loop's
thread and the threads of a gRPC mount's pool now tell the runtime their
names, as the pools for synchronous code do. The main thread is the one
whose TID is the worker's pid, and a kernel name equal to the process's
counts as none, because a thread nothing names inherits its creator's,
and the main thread's is the process's.

Wrapping `Thread._set_os_name` would give whole names for application
threads too, but patches a private method, adds a call into the runtime
for every thread started, and would need the runtime's table pruned as
threads end. Asking the worker cannot be answered by a worker whose
threads hold the interpreter, which is the case reading from outside is
for, and does not know threads `threading` did not start either.

### Consequences

- Good: the event loop's thread, and every thread an application starts
  with `threading`, are named in `muructl debug stacks`, profiles and the
  capture before a hard-timeout kill, on a worker that cannot answer as
  on one that can.

- Good: the cost is one small file read for each thread, a few
  microseconds, in the host.

- Bad: Linux keeps 15 bytes of a name, so a longer name the runtime does
  not have is shown cut, and the threads of a pool whose prefix is long,
  such as `concurrent.futures`' default `ThreadPoolExecutor-0_N`, look
  alike.

- Bad: a thread started with `_thread.start_new_thread`, or by C code
  that does not name it, carries the name of the thread that started it
  when that is not the main thread, and is shown by that name.

## More information

Section 11.7 of the design specification; issue 405; ADR 0028, whose
statement that threads read from outside carry only the runtime's names
this supersedes; the `Names` class of the runtime library's `_outside`
module. A probe of 3.14, 3.14t, 3.15 and 3.15t in a Linux container found
the reader's identifiers to be the TIDs and the kernel to hold each
`threading` name, cut to 15 bytes, and the name of a thread started with
`_thread.start_new_thread` to be its creator's.
