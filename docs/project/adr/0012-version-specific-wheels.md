---
status: accepted
date: 2026-10-23
---

# ADR 0012: Version-specific runtime wheels for every supported CPython, no abi3

## Context and problem statement

The runtime library contains a PyO3 extension. It can be built once against
the stable ABI (abi3) or once per CPython version (specification section
17.1).

## Decision drivers

- Free-threaded builds cannot use the stable ABI.

- The bridges benefit from the full C API where it helps performance.

- One build path for regular and free-threaded builds keeps the release
  workflow simple.

## Considered options

- abi3 wheels for regular builds plus separate version-specific wheels for
  free-threaded builds.

- Version-specific wheels for every supported CPython, regular and
  free-threaded.

## Decision outcome

Chosen option: version-specific wheels (cp311 to cp315, cp314t and cp315t, and
newer versions as they are released) for manylinux_2_28, musllinux_1_2 and macOS on
x86_64 and aarch64, built by maturin-action in the release workflow.

### Consequences

- Good: one build path; full C API available; free-threaded support without a
  special case.

- Bad: a larger wheel matrix to build and test; new Python versions need a
  release before they are usable, which is why pre-releases enter CI as soon
  as release candidates appear.

## More information

Specification sections 6.11, 8.3, 17.1 and 19.3 (runtime wheel matrix risk).
