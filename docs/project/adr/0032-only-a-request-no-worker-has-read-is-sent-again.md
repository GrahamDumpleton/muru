---
status: accepted
date: 2026-10-28
---

# ADR 0032: Only a request no worker has read is sent again

## Context and problem statement

Section 4.8 had the edge retry a request in two cases: a connection
failure before any byte of the request was sent, and an idempotent
method on a worker that died before sending a response head, when the
body was buffered or empty. Neither was built through the first alphas,
so every lost worker connection was answered 502 `worker_crashed`, even
when the worker had read nothing of the request. The workshops after
0.1.0a9 met the plainest form: Ctrl-C on `muructl run` with a browser
open, where a page's next request was written to a worker that had just
closed its link, and the user saw a crash that was a stop. The question
is which of the two cases to build.

## Decision drivers

- A request must never run twice unless it provably did not run once:
  a retried order, payment or write is worse than a 502.

- "Idempotent" is a property of HTTP methods, not of handlers: GET
  handlers write, and a method says nothing about what a request does
  to a worker.

- A request that crashes a worker (bad input, a C extension fault, a
  memory blowup) would crash the next worker it is sent to, and under
  load a few such requests would take a group down worker by worker.

- The failure the workshops showed is the first case alone: a worker
  that has not read the request head has seen nothing of it, so sending
  it elsewhere cannot run anything twice.

## Considered options

- Build both cases as section 4.8 described them.

- Build the first case only: a worker that cannot be connected to, or
  that closes its connection before the head is written, is skipped and
  the request goes to another worker once.

- Build the first case, and offer the second as an opt-in route setting
  with one retry and a budget.

## Decision outcome

Chosen option: the first case only. A failed connect makes the pool try
another worker while one is left; a failed head write sends the request
once more, to another worker of the pool, or over a new connection to
the same one when it is the only worker. A request whose head a worker
has read is never sent again, whatever its method. Section 4.8 is
amended to say so.

The opt-in form of the second case is not offered. It can be added later
as a route setting if a use shows up, but nothing in the design depends
on it, and the default must stay that a request runs at most once.

### Consequences

- Good: a worker that is stopping or has just gone costs no request,
  which closes the Ctrl-C window and the same window during a
  replacement or a scale-in.

- Good: nothing the application did once can be done twice by the edge.

- Bad: a worker that dies with a request in flight still fails that
  request with 502 `worker_crashed`, even for a GET that would have been
  safe; the client retries if it wants to.

- Neutral: retries are visible, in `muru_request_retries_total` and the
  access log's `retried` field, so a worker that keeps refusing
  connections shows up rather than being papered over.

## More information

Section 4.8 of the design specification. The edge's behaviour on a stop
(section 13.4) is what makes the first case rare under the daemon: a
release leaves the snapshot before its host is told to stop.
