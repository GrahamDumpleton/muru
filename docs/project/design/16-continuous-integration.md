# 16. Continuous integration

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

GitHub Actions runs every check on pull requests and main, longer suites nightly, and publishing only from tagged releases. Required checks must pass before merge; slow and environment-heavy suites report but do not block until they are stable.

## 16.1 Workflows

| Workflow | Trigger | Jobs |
| --- | --- | --- |
| ci.yml | pull request, push to main | lint, Rust tests, Python tests, conformance, integration (no Docker), docs build, MSRV check |
| containers.yml | pull request touching container code, nightly | Docker-based integration scenarios on Linux runners |
| nightly.yml | schedule (daily), manual | full integration matrix, fault injection, fuzzing (time-boxed per target, crashes uploaded as artifacts and turned into issues), soak (weekly) |
| bench.yml | manual, nightly on main, pull requests labelled bench | performance scenarios on a dedicated or fixed-size runner, results compared with main |
| security.yml | pull request, weekly | cargo-deny (licences, advisories, bans), cargo-audit, pip-audit, dependency review, CodeQL for Rust and Python |
| release.yml | tag `v*.*.*` | build, test and publish (section 17) |
| docs.yml | pull request | Sphinx build with warnings as errors and link check (Read the Docs builds previews separately) |
| images.yml | release, weekly | build, test, sign and push base images |

## 16.2 Matrix for ci.yml

| Dimension | Values |
| --- | --- |
| Operating system | ubuntu (x86_64), ubuntu arm64 runner, macOS arm64 |
| Python | 3.11, 3.12, 3.13, 3.14, 3.14t, 3.15, 3.15t, and the next release's pre-releases as allowed failures |
| Rust | stable for everything, MSRV for a build check, nightly only for fuzzing |

The full Python matrix runs on Linux x86_64. Other platforms run the newest and oldest supported Python to keep the matrix bounded.

## 16.3 Practices

- Caching: Swatinem/rust-cache for Cargo, uv's cache for Python environments, cached test certificate authorities and fixture builds.
- The extension is built once per job with maturin develop through just dev, and test jobs share the built wheels where possible.
- Integration tests use the same just and nox entry points as local runs, so CI and laptops behave the same.
- Failing integration scenarios upload daemon logs, event streams, access logs and stack dumps as artifacts.
- Required checks: lint, Rust tests, Python tests on Linux, conformance, integration without Docker, docs build. Containers, nightly and bench are informational until stable.
- Dependabot or Renovate updates Cargo, Python and GitHub Actions dependencies weekly. Actions are pinned by commit hash.
- Changelog fragments (towncrier) are required for user-visible changes, checked by a CI job.
