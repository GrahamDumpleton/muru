---
status: accepted
date: 2026-10-23
---

# ADR 0018: The project version is stated once, in Cargo.toml

## Context and problem statement

One tag releases two PyPI distributions, ten crates, a Python package with
`__version__` and a documentation site, all meant to carry the same version
(specification section 17.2). Stating the number in each place invites them
to drift.

## Decision drivers

- A release bump should touch one file.

- Nothing at runtime should read a build file that is not shipped.

## Considered options

- State the version in every file and check consistency in CI.

- State it once in the Cargo workspace and derive the rest.

## Decision outcome

Chosen option: the Cargo workspace `version` is the only place the number is
stated. The crates inherit it. The muru and muructl distributions declare
`dynamic = ["version"]` so maturin copies it into the wheel metadata.
`muru.__version__` reads that metadata through `importlib.metadata`, with a
fallback when the package is imported from the source tree uninstalled. The
documentation reads Cargo.toml directly at build time. The extension carries
its own compiled copy (`CARGO_PKG_VERSION`) so the host can detect a Python
package and extension that do not match. `just bump X.Y.Z` makes the change,
and `just version` prints it.

### Consequences

- Good: no drift; the release checklist names one file.

- Bad: `__version__` is `0+unknown` when the package is used without being
  installed, which only happens in the repository's own pure-Python tests.

## More information

Specification sections 17.2 and 17.4.
