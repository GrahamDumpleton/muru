---
status: accepted
date: 2026-11-06
---

# ADR 0048: A request counts against the side that runs it

## Context and problem statement

A worker that serves WSGI and ASGI has two capacities: its application
threads (`threads`) and its event loop (`concurrency`). The edge held a
request against one of them by the interface of the release's default
mount, not by the interface of the mount that served the request
(section 5.1 as it stood). Out of the box, an ASGI mount beside a WSGI
application shared its four thread slots: twenty requests that each
awaited a second took five seconds, four open WebSockets stopped the
WSGI application, and a WSGI mount beside an ASGI application was held
against the loop's thousand slots while it ran on four threads, so its
requests queued inside the worker, outside the queue timeout. The same
WebSocket was counted on the loop when a WSGI view handed it to ASGI and
on a thread when a mount sent it there by path. Getting an ASGI mount a
place of its own took `interface = "asgi"` and `concurrency`, which a
mount left to be detected could not set. Issue #362 asked for a model
that is easy to explain and whose defaults work without tuning.

Separately, the daemon's autoscaler sized a release from its configured
interface, so one left at `auto` and detected as ASGI was sized as WSGI
(issue #377): one more place where the configured interface was read
after the real one was known.

## Decision drivers

- The rule is one sentence a user can hold: which setting sizes which
  requests.

- A mixed application behaves reasonably with no setting beyond its
  entrypoints.

- A request is counted the same way whichever route took it to a side,
  a mount or a handoff.

- `interface = "auto"` stays, since detection is what makes the first
  run of a FastAPI or Django application work, without each new piece
  of code having to remember which interface to read.

## Considered options

- Keep counting against the default mount's side, and document the
  tuning.

- Count each request against the side that runs it, with the interface
  of each mount resolved once and announced.

- Require `interface` on every mount, keeping `auto` only for the
  release's entrypoint.

- Remove `auto`, with the interface defaulting to `wsgi`.

## Decision outcome

Chosen option: "Count each request against the side that runs it",
because it is the only one that fixes the counting, and it needs nothing
of the user.

- A WSGI request, the default mount's or a named mount's, counts
  against the worker's threads. An ASGI or gRPC request counts against
  its event loop's concurrency. An open WebSocket is on the loop.

- The worker declares in HELLO the threads of its shared pool when
  anything it serves runs on them, its loop's concurrency when it has a
  loop, and for every named mount the side it runs on. The edge holds a
  count for each side, takes a request's place on the count of its
  mount's side, and chooses the worker with the most places free on
  that count. A mount's own `threads`, `concurrency` and
  `max_connections` stay as optional reserved pools, and a mount that
  borrows shared threads borrows from the thread count.

- Handoff is unchanged: DETACH moves a request from one count to the
  other.

- The interface each entrypoint runs as is resolved once, by the host
  at import or by each worker for a factory, and announced. Everything
  after start-up uses the announced interface: the edge's counting, the
  checks of a mount's own threads and limits, the autoscaler, the
  refusal to trace a busy worker, and the listing of a release. In the
  daemon one accessor answers "the interface this release or mount runs
  as", and the configured value is its fallback only until the host
  has announced.

- A mount's `concurrency` and `max_connections` are accepted with the
  interface left to be detected, and a mount detected as anything but
  ASGI with either fails its worker's start, as a mount detected as
  ASGI with threads already did. A mount declared WSGI or gRPC is
  refused them when the configuration is checked.

- A group whose workers have both sides is as busy as its busier side:
  the one with the larger share of its slots in use, which runs out
  first. The scaling rules, the diagnosis and
  `muru_capacity_utilisation` read that side. `muru_slots_busy` and
  `muru_slots_total` carry a `side` label, `threads` or `loop`, and
  `muru_async_slots_busy` is gone, being the `loop` series.

Requiring `interface` on every mount would bring a mount's limit errors
forward from a worker's start to `muructl apply`, at the cost of a line
per mount and of the release and its mounts following different rules;
it fixes none of the counting. Removing `auto` would make every ASGI
application fail its first run until a line is added, and the error
would come no earlier, since only the import shows what a callable is.
Both stay open if resolving `auto` once proves hard to keep to.

### Consequences

- Good: an ASGI mount beside a WSGI application, and the reverse, need
  no settings; the how-to is "WSGI requests count against `threads`,
  ASGI requests against `concurrency`".

- Good: a WSGI mount beside an ASGI application queues at the edge,
  under the queue timeout and the least-outstanding choice of worker,
  where it queued unseen inside the worker.

- Good: a mount sharing its side's count is declared in HELLO, so a
  runtime library older than the edge, which names no such mount, is
  counted as before, against the default mount's side.

- Bad: the slot metrics change shape. A query that read
  `muru_slots_busy` as one series per group gets two for a group with
  both sides, and `muru_async_slots_busy` no longer exists.

- Bad: a mount's limit on the wrong interface is found when a worker
  starts, under the daemon during the release's warm-up, when the
  interface is left to be detected.

## More information

Issues #362 and #377. Sections 5.1, 5.6, 6.7, 6.14, 7.3 and 11.1 of the
specification. ADR 0023 and ADR 0042 for the handoff, whose accounting
this record extends to mounts.
