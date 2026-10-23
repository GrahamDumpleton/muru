---
status: accepted
date: 2026-10-23
---

# ADR 0009: wsgi.input_terminated is always true and wsgi.multiprocess reflects overlap

## Context and problem statement

PEP 3333 leaves the handling of bodies without Content-Length to the server,
and frameworks read wsgi.multiprocess and wsgi.multithread to decide how to
behave (specification section 6.5).

## Decision drivers

- Frameworks that only read CONTENT_LENGTH must work.

- Releases overlap during deploys, recycles and scaling, so an application
  can never assume a single process in daemon mode.

## Considered options

- Pass chunked bodies through and leave wsgi.input_terminated unset.

- wsgi.input_terminated always true; chunked bodies buffered by default so
  CONTENT_LENGTH is set, with stream and reject as alternatives.

- wsgi.multiprocess true only when more than one worker is configured.

- wsgi.multiprocess true for any daemon-managed release, false only for
  single-process development runs, with both flags forceable per release.

## Decision outcome

Chosen options: the second and the fourth. wsgi.input always returns an empty
value at the end of the body, wsgi.multithread is true when the serving pool
has more than one thread, and wsgi.run_once is false.

### Consequences

- Good: frameworks behave correctly without Muru-specific code; applications
  do not mistake a one-worker release for a single process.

- Bad: buffering chunked bodies costs memory or disk up to the body limit;
  streaming routes must opt in.

## More information

Specification sections 4.6 and 6.5.
