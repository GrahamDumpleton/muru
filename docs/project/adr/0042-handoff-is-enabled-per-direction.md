---
status: accepted
date: 2026-11-01
---

# ADR 0042: Handoff between WSGI and ASGI is enabled per direction in configuration

## Context and problem statement

ADR 0023 lets a request be handed from WSGI to ASGI, or from ASGI to WSGI,
inside a worker, and made the presence of `muru.wsgi_to_asgi` in the
environ the feature test: it is there when the worker happens to have an
event loop. Whether a worker has a loop, or a thread pool beyond one
thread, depends today on what else the release serves: a WSGI release
gains a loop only by also mounting an ASGI application, and an ASGI
release runs one thread. An application would find out whether its
handoff works by what the rest of the configuration did, and the edge
could not count the threads an ASGI application hands requests to,
because none were declared. Building the handoff (issue #91) needs those
answers fixed. This record replaces the feature test of ADR 0023 and
settles how handed-off requests are counted; the rest of ADR 0023
stands.

## Decision drivers

- Whether a request may be handed across, and in which direction, is
  visible in the configuration file, not inferred.

- An application that uses a handoff not enabled finds out with an error
  that says how to enable it, at the first request, not with a key that
  is silently missing.

- A worker starts an event loop or a thread pool only when something it
  serves needs it.

- The edge counts every request against the side that runs it, without
  a second kind of worker and with the link unchanged apart from the
  DETACH frame section 5.2 reserves.

## Considered options

- Infer: the key is present when the worker has a loop or threads, as
  ADR 0023 has it.

- A loop in every WSGI worker and threads in every ASGI worker, so both
  keys always work.

- A setting per direction on each release and mount, with the keys
  always present under Muru and raising when the direction is not
  enabled.

## Decision outcome

Chosen option: a setting per direction, with the keys always present.

- **The setting.** `handoff` on a release's own entry or on a mount names
  the interface its requests may be handed to: `handoff = "asgi"` on a
  WSGI application, `handoff = "wsgi"` on an ASGI one. A WSGI
  application can only hand to ASGI and the reverse, so the value says
  the direction in the file where it is read. A mount whose interface,
  declared or detected, is the one it names fails its worker's start
  with an error naming the mount. gRPC mounts take no handoff.

- **What it starts.** `handoff = "asgi"` anywhere in a worker starts its
  event loop if nothing else has. `handoff = "wsgi"` makes `threads`
  valid on an ASGI release and as a number on an ASGI mount: the
  release's threads become the worker's shared pool, which an ASGI
  release otherwise runs as one thread, and a mount's are its own pool.
  Without `threads` the release's default applies.

- **Keys always present.** Under Muru every WSGI environ has
  `muru.wsgi_to_asgi` and every ASGI http scope has
  `extensions["muru"]["asgi_to_wsgi"]`. When the request's mount does not
  enable that direction, `handoff()` raises `muru.HandoffNotEnabled`, a
  RuntimeError, naming the mount and the setting to add. Outside Muru
  the keys are absent, as under any other server.

- **WebSockets follow the setting.** A WSGI mount receives a WebSocket
  upgrade as a websocket request only when it has `handoff = "asgi"`;
  without it the upgrade is an ordinary request, whatever else the
  worker serves. The worker says which of its mounts take websocket
  requests in HELLO, in place of one flag for the worker.

- **Counting.** The edge keeps, for each worker, a count for its threads
  and one for its event loop, each bounded by what HELLO declares, and a
  request is dispatched against the side its mount runs on. When it is
  handed across, the worker sends DETACH and the edge moves the request
  to the other count. A request is never refused or held at that point,
  since it is already running: if the other count is full, the request
  takes its place anyway and the count is over by one, and the edge
  dispatches nothing new against that count until it is back within its
  bound. A WebSocket accepted on the loop already gives its request
  slot back, as it does today, so a WebSocket handoff needs no DETACH.

- **Waiting for a thread.** A request handed to WSGI while every thread
  is busy waits in the worker, in order, until a thread frees or its
  deadline passes, when it is answered 503 with reason `queue_full`. The
  count rule above keeps that wait to the handoffs made while the
  threads were full. A request handed to ASGI never waits; the loop
  takes any number of tasks.

- **Lifespan.** `muru.asgi.register(app)` never starts an event loop;
  the setting is what starts it. A registration from post_fork, which
  runs before the loop, waits for the loop, and a worker with a
  registration but no loop fails to start with an error naming the
  application.

### Consequences

- Good: the configuration says what a release may do, and a missing
  setting is an error that names its fix, not a handoff that works in
  one release and not in another depending on its mounts.

- Good: an ASGI application that hands work to WSGI has a thread pool
  sized in its configuration and counted by the edge, which ADR 0023
  left open.

- Good: a WSGI path in a worker that also serves ASGI stops receiving
  WebSocket requests it cannot accept.

- Bad: a WSGI application that adds a WebSocket view needs a line of
  configuration as well as the code.

- Bad: a count can run one over its bound for each handoff made while it
  was full, which the edge corrects by holding new dispatches; it never
  refuses a request already running.

## More information

Specification sections 5.1, 5.2, 6.7, 6.9, 6.14 and Appendix D.
Supersedes the feature test of ADR 0023 (the presence of
`muru.wsgi_to_asgi` when the worker has an event loop); ADR 0023 is
otherwise in force. Issues #91, #273, #274 and #275.
