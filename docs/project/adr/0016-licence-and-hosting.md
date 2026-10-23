---
status: accepted
date: 2026-10-23
---

# ADR 0016: Apache-2.0 licence, GitHub repository and GitHub Container Registry

## Context and problem statement

The project needs a licence, a home for the source and a registry for base
images before the first public release (specification sections 17.1 and
19.1).

## Decision drivers

- A permissive licence with an explicit patent grant, familiar to companies
  deploying Python applications.

- One account for source, issues, discussions, security advisories, CI and
  images.

## Considered options

- MIT or BSD: permissive but without a patent grant.

- Apache-2.0.

- A separate organisation for the repository and images.

## Decision outcome

Chosen option: Apache-2.0, with the repository at
github.com/GrahamDumpleton/muru and images at ghcr.io/grahamdumpleton. The
PyPI names muru and muructl are to be registered with placeholder releases
before the first public announcement.

### Consequences

- Good: a well understood licence; one place for everything the project
  publishes; trusted publishing and signed images fit the platform.

- Bad: moving to an organisation later would change image and documentation
  URLs.

## More information

Specification sections 17.1, 18.4 and 19.1.
