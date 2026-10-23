---
status: accepted
date: 2026-10-23
---

# ADR 0001: gRPC over Unix sockets for the edge-to-worker link

## Context and problem statement

The edge parses HTTP once and must hand each request to a Python worker as
typed frames, with streaming in both directions, flow control, cancellation
and deadlines. The choice of protocol on this hop shapes the Rust extension,
the benchmarks and the upgrade story (specification section 5).

## Decision drivers

- No HTTP re-serialisation between edge and application, which removes
  request smuggling and desync risks on this hop.

- Bidirectional streaming with credit-based flow control, cancellation,
  deadlines, trailers and status codes without building them from scratch.

- A mature Rust stack on both ends, and a protocol that can be versioned with
  a compatibility window.

- Fork safety: nothing in the worker side may start threads before fork.

## Considered options

- HTTP/1.1 over a Unix socket: simple and universal, but no multiplexing, a
  re-serialisation risk, and no typed control channel.

- A custom framed protocol: the smallest overhead, but flow control,
  cancellation, versioning and tooling would all be custom.

- gRPC with tonic and prost over Unix sockets.

## Decision outcome

Chosen option: gRPC with tonic and prost over Unix sockets, with the worker
side a tonic server inside the Rust extension started after fork. grpcio is
never used for the internal link, because its C core threads conflict with
preload and fork. Both ends sit behind a transport trait so a framed protocol
can replace gRPC if the phase 0 benchmarks require it.

### Consequences

- Good: streaming, flow control, cancellation, deadlines, health checking and
  versioning come with the stack; the protocol is self-describing and easy to
  evolve additively.

- Bad: HTTP/2 framing and protobuf costs on every request, which need tuning
  (Unix sockets only, 1 MiB windows, 64 to 256 KiB body frames, zero-copy
  bytes, coalescing of small chunks) and must be measured against gunicorn,
  uvicorn, Granian and mod_wsgi-express in phase 0.

## More information

Specification sections 5.1 to 5.8 and 14.5 (phase 0 exit criteria). Open
question 1 in section 19.2 records the benchmark threshold still to be agreed.
