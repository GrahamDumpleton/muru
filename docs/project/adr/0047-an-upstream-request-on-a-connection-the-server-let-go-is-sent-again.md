---
status: accepted
date: 2026-11-04
---

# ADR 0047: An idempotent upstream request on a connection the server let go is sent again

## Context and problem statement

The edge keeps HTTP/1.1 connections to an upstream or a managed HTTP
server between requests. A server may close a kept connection at any
time, and does as a matter of course: Apache's event MPM closes kept
connections when it has no thread free, and at its keep-alive timeout of
a few seconds against the edge's 60. When the close crosses a request
the edge has just written on that connection, nothing comes back, and
the edge answered 502 `upstream_failed`. In the muru-benchmarks size
suite that was about 3 percent of requests to mod_wsgi-express in
embedded mode (#329).

ADR 0032 settled that a request a worker has read is never sent again,
whatever its method, because a request must not run twice unless it
provably did not run once. The edge cannot prove that a server which
closed the connection under a request did not read it. The question is
whether the same rule holds for upstreams.

## Decision drivers

- A server letting a kept connection go is ordinary HTTP/1.1 behaviour,
  not a fault, and a client cannot see it coming: RFC 9112 section 9.3.1
  lets a client retry an idempotent request when the connection closes
  before any response, and RFC 9110 section 9.2.2 forbids it only for
  non-idempotent methods. nginx retries in this case by default.

- The failure is frequent with common servers and cannot be configured
  away on the edge's side: honouring the server's `Keep-Alive: timeout`
  removes the timeout case only.

- ADR 0032's other driver, a request that crashes a worker crashing the
  next, does not arise: the retry follows a close on a connection kept
  from an earlier request, not a failure the request caused, and it is
  made once, on a new connection.

- A request on a connection opened for it, or one that got any part of a
  response, says the server took it, and is never sent again.

## Considered options

- Send the request again, once, on a new connection to the same address,
  when its method is idempotent and its body is held whole or empty.

- The same, only when an upstream or profile turns it on.

- Never send it again; honour `Keep-Alive: timeout` only.

## Decision outcome

Chosen option: send it again by default. A request that went out whole
on a connection kept from an earlier request, where the connection then
closed or was reset with nothing read back, goes once more on a new
connection to the same address when its method is GET, HEAD, OPTIONS,
TRACE, PUT or DELETE and the edge holds its body whole or it has none.
The address's other kept connections, idle as long, are let go, and the
close does not count against the address's health. The retry is marked
in the access log and counted in `muru_request_retries_total`. Kept
connections are also used for no longer than a second less than the
server's `Keep-Alive: timeout`, or not kept when that leaves none.

ADR 0032 stands for workers, whose link is Muru's own and where a
request a worker has read is never sent again.

### Consequences

- Good: a server letting kept connections go costs no requests that HTTP
  says may be repeated.

- Bad: an idempotent request whose handler is not, such as a GET that
  writes, can run twice when a server read it and closed the connection
  without answering. A POST or PATCH, or a request whose body was
  streamed, is still 502.

- Neutral: the retries show in metrics and the access log, so a server
  that closes connections often is visible rather than hidden.

## More information

Section 4.13 of the design specification; issue #329; ADR 0032.
