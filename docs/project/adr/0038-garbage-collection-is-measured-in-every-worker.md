---
status: accepted
date: 2026-10-31
---

# ADR 0038: Garbage collection is measured in every worker

## Context and problem statement

CPython's cyclic garbage collector pauses the whole process while it runs:
the collecting thread holds the GIL on a regular build, and the collector
stops every thread on a free-threaded one. In a worker the pause lands on
whatever requests are in flight, as tail latency no single endpoint
explains. The host already calls `gc.freeze()` before every fork
(section 6.2), so a worker's collections scan only what the worker itself
created, but nothing recorded what collection still cost: `muructl debug
memory` showed the gc counts at one moment and no more (sections 11.1 and
11.7).

## Decision drivers

- An operator should be able to see whether collection costs an
  application anything before deciding to do anything about it.

- A measurement that is off by default is one nobody turns on until after
  the incident.

- Generations differ across the supported interpreters: the free-threaded
  build collects the whole heap each time, and the regular build's layout
  has changed between releases.

- Muru deciding when collection runs (between requests, after a warm-up,
  or coordinated across a group) is a larger change that the measurement
  should inform first.

## Considered options

- Leave it to applications, which can register their own `gc.callbacks`
  and export the results themselves.

- Measure in every worker through `gc.callbacks`, on by default, and
  report through the status line the worker already writes.

- The same, as a setting off by default.

## Decision outcome

Chosen option: the second. The runtime registers one callback in each
worker after fork. On `start` it notes a monotonic time and the requests
the worker has in flight; on `stop` it adds the pause to a histogram over
the signal bounds of section 7.3, kept apart by generation and by whether
requests were in flight, and adds the objects freed. The figures ride the
status line with the CPU time and GIL waits, the daemon exports them for
each worker (counters of collections, seconds and objects freed by
generation, and the pause histogram by generation and `requests`), and
`muructl debug memory`, the dashboard's worker view and the scaling
diagnosis read them. The generation label is what the interpreter reports:
`0`, `1` or `2`, or `full` on a free-threaded build.

The callback costs two calls into the extension, one clock read and a
few atomic additions per collection, measured at about a microsecond,
where an automatic collection scans at least a threshold's worth of new
objects and takes tens of microseconds or more, so it is on with no
setting. The
diagnosis names collection when it takes a tenth of the workers' time or
more, a provisional threshold to revisit against real applications.

### Consequences

- Good: the cost of collection is visible per worker and per generation,
  with the part that landed on requests told apart from the part that cost
  nothing, so the later question of whether Muru should take control of
  when collection runs can be answered from figures.

- Good: nothing to configure, and nothing for an application to add.

- Bad: a few more fields on every status line and, with histograms per
  generation and per in-flight state, a few hundred more samples per
  worker in the exposition.

- Bad: an application that registers its own `gc.callbacks` sees Muru's
  beside it; the callback takes no action, so the two do not interfere.

## More information

Sections 6.2, 7.3, 11.1 and 11.7 of the specification; issue #189, whose
closing note lists the options for Muru deciding when collection runs,
none of which is in scope here.
