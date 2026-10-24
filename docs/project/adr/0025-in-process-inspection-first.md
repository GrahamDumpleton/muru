---
status: accepted
date: 2026-10-25
---

# ADR 0025: Inspect workers from inside the process first

## Context and problem statement

Section 11.7 describes two mechanisms for looking into a worker: from
outside the process with PEP 768 on Python 3.14, which needs no
cooperation from the worker and so works on one that is hung, and from
inside, through a control thread in the runtime library that attaches to
the interpreter only to run a named operation. It places status, stacks,
tasks, inspect, logger levels and faulthandler in phase 2 and says the
stacks are "taken from outside". Phase 2 has to ship inspection on every
supported interpreter, from 3.11 up, and the outside reader exists only
on 3.14.

## Decision drivers

- Every supported interpreter gets the same commands.

- A hung worker still yields its stacks somehow.

- The path from daemon to host to worker is what later phases build on
  (profiling, attach, exec), so it is worth having first.

## Considered options

- Build the PEP 768 reader first, with the in-process fallback for older
  interpreters.

- Build the in-process control thread first, with faulthandler as the
  fallback for a worker that cannot answer, and add the PEP 768 reader
  later as a second source for the same commands.

## Decision outcome

Chosen option: the in-process control thread first. Every worker gets a
control connection to its host at fork; a thread of the runtime library
reads requests from it, attaches to the interpreter to answer, and
writes the answer back. The host relays the daemon's requests and
answers for a worker that does not reply within five seconds, sending it
SIGQUIT so that faulthandler writes its stacks to the log. The daemon's
Debug operation carries status, stacks, tasks, inspect and logging
through the host's control socket.

### Consequences

- The commands work on every supported interpreter from phase 2.

- A worker whose interpreter is held cannot answer in detail; its stacks
  reach the log through faulthandler rather than the command's output.
  The PEP 768 reader, when added for 3.14 and later, will take the
  stacks from outside and keep the output whole; section 11.7's
  description of stacks "taken from outside" is the target, and this
  record notes the interim.

- The control connection is a socket pair inherited at fork and a JSON
  line protocol, not yet the gRPC Worker service of section 5.9; the
  daemon never addresses a worker directly, which the gRPC service would
  allow later without changing the commands.
