---
status: accepted
date: 2026-11-02
---

# ADR 0045: The repositories and images live under the muruhq organisation

## Context and problem statement

ADR 0016 put the repository at github.com/GrahamDumpleton/muru and the
images under ghcr.io/grahamdumpleton, and noted that moving to an
organisation later would change image and documentation URLs. Muru is
aimed at organisations running Python web applications in production,
and the next step of the containers goal publishes the first images and
binaries (specification sections 17.1 and 19.1).

## Decision drivers

- Organisations adopting Muru look for a project home that does not
  depend on one person's account.

- Image references get pinned in other people's Dockerfiles once images
  are published, so the registry path has to be settled before then.

- A home that can later carry support or other paid offerings without
  another move.

## Considered options

- Stay under the personal account.

- Move muru and muru-workshops to the muruhq organisation before any
  image is published.

## Decision outcome

Chosen option: move to the muruhq organisation, because no images were
published yet and there were no users, so nothing outside the
repositories needed to change. The repositories are github.com/muruhq/muru
and github.com/muruhq/muru-workshops, images go to ghcr.io/muruhq, and
PyPI trusted publishing names the muruhq owner. The documentation is
served at docs.muruhq.dev, with muru.readthedocs.io redirecting there.
The licence stays Apache-2.0, as ADR 0016 decided. The project board
stays under the personal account, where it holds the organisation's
issues.

### Consequences

- Good: the project presents as an organisation's, and image paths are
  final from the first published image.

- Bad: old links rely on GitHub's redirects, which break if a repository
  called muru is ever created under the personal account again.

## More information

Supersedes the hosting part of [ADR 0016](0016-licence-and-hosting.md).
Specification sections 17.1 and 19.1.
