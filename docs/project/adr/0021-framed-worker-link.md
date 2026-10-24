---
status: accepted
date: 2026-10-24
---

# ADR 0021: A framed protocol with protobuf payloads for the edge-to-worker link

## Context and problem statement

ADR 0001 chose gRPC over Unix sockets for the link between the edge and
Python workers, on condition that the phase 0 spike measured its cost and
that a framed protocol could replace it if the cost was too high. The spike
built both: a tonic server in the worker with tuned HTTP/2 windows and a
small pool of multiplexed connections, and a framed link with one request
per pooled connection. Specification section 5 describes the result.

## Decision drivers

- Per-request cost at the level of mod_wsgi daemon mode or better, measured
  against gunicorn, uvicorn, Granian and the three mod_wsgi-express
  configurations in section 15.4.

- Streaming in both directions, cancellation, trailers and versioning, with
  as little machinery as the link needs and no more.

- One protocol for WSGI, ASGI and WebSockets, with the edge acting on
  declared capabilities rather than worker kinds.

- No HTTP re-serialisation and no request smuggling surface on the hop.

## Considered options

- gRPC with tonic and prost, as in ADR 0001, with tuning: 2 MiB stream and
  16 MiB connection windows, 1 MiB frames, zero-copy bytes, two connections
  per worker.

- A framed protocol: a six byte header (length, kind, flags), protobuf
  messages for structured payloads and raw bytes for bodies, one request per
  connection, a pool of persistent connections per worker bounded by the
  worker's declared slots.

- A shared-memory transport (memfd ring buffers with futex signalling),
  considered and deferred to section 20.7.

## Decision outcome

Chosen option: the framed protocol, with gRPC retained for the control plane
between daemon and worker (section 5.9) and for the admin API.

The phase 0 measurements, requests per second with four workers of four
threads and 32 connections, median of three runs, on a four-core Linux
virtual machine:

| Scenario | gRPC | Framed | mod_wsgi daemon | mod_wsgi embedded | gunicorn gthread | Granian |
| --- | --- | --- | --- | --- | --- | --- |
| hello, 14 bytes | 22.9k | 63.7k | 31.6k | 90.6k | 65.2k | 234.2k |
| large, 1 MiB | 2.9k | 4.6k | 2.9k | 16.2k | 8.6k | 8.6k |
| chunks, 1000 small yields | 1.7k | 2.0k | 2.7k | 1.7k | 1.2k | 2.0k |

gRPC sat 24 to 37 percent behind mod_wsgi daemon mode on hello world after
tuning, and its HTTP/2 layer caused a failure mode of its own: hyper drops a
response body once the declared length is written, which reset gRPC streams
before their trailers and, after 1024 resets, closed the connection. The
framed link is at the level of mod_wsgi embedded mode and gunicorn per
request, twice daemon mode, and keeps every byte of large bodies moving at
rates above a 25 GbE link. Its remaining gap to in-process servers is one
copy per byte, which does not matter behind a real network and is removed
for files by descriptor passing.

What gRPC provided is replaced as follows: flow control by the socket
buffer, since nothing shares a connection; cancellation by the ABORT frame;
trailers by the END frame; versioning and capabilities by HELLO; status by
the control plane. The frames for WebSockets, informational responses and
descriptor passing are reserved in section 5.2 so that the one protocol
serves every worker kind.

### Consequences

- Good: per-request cost no longer a concern on the roadmap; a protocol
  small enough to read in one sitting, with both ends in about a thousand
  lines of Rust; one request per connection makes back-pressure and cancellation
  local to a connection; WebSockets and files ride the same frames.

- Good: the worker no longer needs an HTTP/2 implementation, and tonic
  leaves the request path entirely.

- Bad: a pool of connections per worker instead of two, so descriptor counts
  on both sides grow with concurrency, which matters only for long-lived
  WebSockets and is bounded by the declared caps.

- Bad: framing, HELLO negotiation and descriptor passing are Muru's own code
  to test and version, with no external tooling to inspect the link.

## More information

Specification sections 5.1 to 5.9, 4.6 (bodies the application does not
read), 6.14 (handing requests between WSGI and ASGI) and 20.7. The benchmark harness is
bench/run.py and the comparison set is section 15.4. This record supersedes
ADR 0001 for the request link; the control plane keeps gRPC as that record
intended. Open question 1 of section 19.2 is resolved by this record.
