# Contributing

Muru takes contributions as requirements, not as code. Pull requests are
limited to collaborators; everyone else contributes by describing what they
need, and the maintainer implements accepted work with AI assistance,
crediting the person who raised it and asking them to test it.

1. Search the existing issues and discussions first.

2. Open a feature request describing the problem, the use case and the
   desired behaviour. A clearly stated requirement is more useful than a
   patch.

3. The maintainer discusses it in the issue and decides whether and how it
   fits the design, recording significant decisions as
   [architecture decision records](adr/index.md).

Bug reports follow the same path through the issue forms. Questions and
open-ended ideas go to GitHub Discussions. Security issues go through private
security advisories, as described in the [security policy](security.md).

## Building and testing

The repository's `CONTRIBUTING.md` and `AGENTS.md` files hold the
prerequisites, the `just doctor` and `just bootstrap` setup, the test
targets and their markers, and the coding conventions. In short:

```
just doctor       # check prerequisites
just bootstrap    # install toolchains and the development environment
just dev          # build the extension into the development virtualenv
just test         # Rust and Python unit tests
just lint         # formatting, lints, types and repository text checks
just docs         # build this documentation
```

## Design first

Read the relevant pages of the [design specification](design/index.md)
before working on a subsystem. When the implementation needs to differ from
the specification, add a decision record and update the specification in the
same change.
