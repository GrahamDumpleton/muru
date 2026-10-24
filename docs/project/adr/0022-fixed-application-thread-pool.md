---
status: accepted
date: 2026-10-24
---

# ADR 0022: A fixed pool of long-lived application threads with persistent thread states

## Context and problem statement

The phase 0 spike ran WSGI requests on the tokio blocking pool, attaching to
the interpreter for each request. Measured behaviour: the same operating
system thread served every request, but each request got a new Python
thread state, so a value kept in threading.local never survived from one
request to the next. The cause is PyGILState_Ensure creating a thread state
for a thread that has none and PyGILState_Release destroying it again, and
tokio's blocking pool in any case retiring idle threads. Specification
section 6.5 needs to say what a request runs on.

## Decision drivers

- Applications and libraries keep per-thread resources in threading.local:
  Django's database connections, SQLAlchemy's scoped sessions, parsers and
  caches. mod_wsgi deliberately reuses threads so these persist; servers that
  spawn a thread per request break them and reconnect on every request.

- Per-request values must not leak between requests on a reused thread.
  Context variables set by one request (request ids, logging bindings, error
  reporting scopes) are a known hazard in every thread-pool server.

- The thread count is also the slot count the edge enforces, so it must be
  fixed and known.

- Free-threaded builds must get the same behaviour from the same code.

## Considered options

- The tokio blocking pool with per-request attach, as in the spike.

- A fixed pool of Rust threads, each attached once with a persistent thread
  state for the life of the worker, taking requests from a queue and
  detaching while idle.

- Real Python threads created with the threading module, each calling into
  Rust for its next request, which is the structure section 6.5 originally
  sketched.

## Decision outcome

Chosen option: the fixed pool of Rust threads with persistent thread states,
each request run inside a fresh copy of the thread's contextvars context
(PyContext_CopyCurrent entered before the application is called and exited
after close()). Thread-locals persist across requests on a thread; context
variables do not.

Real Python threads were not chosen because the worker's threads are created
in Rust after fork, before any Python runs in the worker, and a Rust thread
that attaches once is the same thing to the interpreter apart from how
threading.current_thread() names it. That can be revisited if a library is
found to depend on the thread object rather than the thread state.

### Consequences

- Good: mod_wsgi's thread reuse semantics; per-thread caches and connection
  pools work; no per-request thread state creation; the WebSocket handler
  form of ADR 0023 can borrow these threads per message.

- Good: contextvar leakage between requests cannot happen, and the same
  context mechanism gives a delegated WebSocket one context across its
  callbacks.

- Bad: a thread blocked in C code holds its slot until it returns; the soft
  timeout (section 7.6) is the remedy, as before.

- Bad: threading.current_thread() reports a dummy thread object for an
  application thread, as it does under mod_wsgi.

## More information

Specification section 6.5. The integration tests cover thread-local
persistence and per-request contexts. Measured in the phase 0 spike on
CPython 3.15 with PyO3 0.30.
