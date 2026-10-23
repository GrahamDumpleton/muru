# Contributing to Muru

## How contributions work

Muru takes contributions as requirements, not as code. Pull requests are
limited to collaborators. Everyone else contributes by describing what they
need:

1. Search the existing issues and discussions first.

2. Open a feature request describing the problem, the use case and the
   desired behaviour. Implementation suggestions are welcome but not required.
   A clearly stated requirement is more useful than a patch.

3. The maintainer discusses it in the issue, decides whether and how it fits
   the design, and records significant design decisions as architecture
   decision records.

4. Accepted work is implemented by the maintainer, with AI assistance, and
   the person who raised it is credited in the changelog and asked to test it.

Bug reports follow the same path through the issue forms. Questions and
open-ended ideas go to GitHub Discussions. Security issues go through private
security advisories, as described in [SECURITY.md](SECURITY.md).

Muru is developed with extensive use of AI coding agents, directed and
reviewed by its creator. See the About page in the documentation for the full
statement.

## Building and testing

The aim is a short list of system tools; everything else is installed per
project by rustup, cargo and uv at versions pinned in the repository.

| Tool | macOS | Linux (Debian or Ubuntu) |
| --- | --- | --- |
| C toolchain and git | `xcode-select --install` | `apt install build-essential pkg-config git` |
| rustup | `brew install rustup`, then `rustup-init` | the installer at rustup.rs |
| uv | `brew install uv` | the installer at astral.sh/uv |
| just | `brew install just` | `apt install just`, or `cargo install just` |

Then:

```
just doctor       # checks every prerequisite and prints install commands for what is missing
just bootstrap    # installs Rust components, cargo tools, Python interpreters and the dev environment
just dev          # builds the extension into the development virtualenv
just test         # Rust and Python unit tests
just lint         # rustfmt, clippy, ruff, mypy and repository text checks
just docs         # builds the documentation
```

Tests that need Docker, network access, root or a long time carry pytest
markers (`requires_docker`, `requires_network`, `requires_root`, `slow`) and
are skipped by default. Tests that need Linux carry `requires_linux` and are
skipped on macOS; run them in the Linux virtual machine with `just vm-up` and
`just vm-test`, or rely on CI, which runs the full Linux matrix on every pull
request.

Not needed: Node.js, OpenSSL, protoc, a system SQLite or a system Python.

## Design specification and decision records

The design specification lives in
[docs/project/design](docs/project/design/index.md) and is the source of
truth for intended design. Read the relevant pages before working on a
subsystem. Architecture decision records live in
[docs/project/adr](docs/project/adr/index.md). When implementation needs to
differ from the specification, add an ADR and update the specification in the
same change.

## Conventions

The rules for code style, layout, typing, docstrings, commits and branches are
in [AGENTS.md](AGENTS.md). They apply to people as much as to AI coding
agents. The ones that can be checked mechanically are checked in CI and by
git hooks, which `just bootstrap` installs from the pre-commit
configuration.

## Changelog

User-visible changes need a changelog fragment under `changelog.d/`. See
[changelog.d/README.md](changelog.d/README.md).
