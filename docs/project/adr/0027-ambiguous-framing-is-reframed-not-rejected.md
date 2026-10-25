---
status: accepted
date: 2026-10-25
---

# ADR 0027: A request with both Content-Length and Transfer-Encoding is re-framed, not rejected

## Context and problem statement

Section 4.13 says that requests to HTTP upstreams are strictly normalised:
a message carrying both Content-Length and Transfer-Encoding, an invalid
Content-Length or obsolete line folding is rejected, and bodies are always
re-framed by the edge. The point is request smuggling: an upstream must
never see a message two parsers could frame differently. The edge reads
HTTP/1.1 with hyper, which settles the question before the edge sees the
request: an invalid Content-Length or folded header line is answered 400
by hyper itself, and a message with both headers is read by its
Transfer-Encoding with the Content-Length header removed, as RFC 9112
section 6.3 permits a server to do. hyper gives the edge no view of the
headers as they arrived, so the edge cannot tell such a message from one
that was chunked all along. The question is whether to keep hyper's
reading or to find a way to refuse the message.

## Decision drivers

- No ambiguous framing may reach an upstream or a worker, which is what
  the rejection in the specification is for.

- The edge should not carry its own HTTP/1.1 parser beside hyper's for
  one check, and hyper's behaviour here is the standard one.

- A client that sends both headers is almost always a broken or hostile
  one; what it gets back matters less than what the upstream gets.

## Considered options

- Keep hyper's reading: the body is taken as chunked and the edge sends
  the upstream one Content-Length and no Transfer-Encoding, so the
  message the upstream sees has one framing.

- Refuse the message by inspecting the raw request head before hyper
  parses it, with a parser of the edge's own in front of hyper.

- Ask hyper for an option to reject such messages, and reject once it
  exists.

## Decision outcome

Chosen option: keep hyper's reading. The edge checks for both headers
itself as well, and refuses the request with 400 `malformed` should a
later hyper ever hand it one; today hyper removes the Content-Length
first. Every body the edge sends to an upstream is re-framed: a body
without a declared length is read in full and sent with a Content-Length,
and a declared length is sent as a Content-Length with the bytes counted
against it. Invalid Content-Length values and obsolete line folding are
refused by hyper with 400 before routing, which the edge relies on. The
specification's wording stands as the intent; this record notes that the
rejection is hyper's where hyper rejects, and a re-framing where it does
not.

### Consequences

- Good: no second HTTP/1.1 parser, and the upstream never sees a message
  with two framings.

- Bad: a client sending both headers gets its chunked body served rather
  than a 400, which is permitted by the RFC but not what section 4.13
  says. The integration tests cover the re-framing.

## More information

Section 4.13 of the design specification; the `upstream` module of the
edge; RFC 9112 section 6.3 on message body length.
