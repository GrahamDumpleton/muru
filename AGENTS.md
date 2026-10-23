# Instructions for AI coding agents

These rules are written in the imperative for AI coding agents and apply
equally to people. They come from section 14.4 of the design specification
and are repeated here so that an agent needs nothing else to start work.

## Start here

1. Run `just doctor`, then `just bootstrap`. Fix anything `just doctor`
   reports as missing before going further.

2. Read the relevant pages of the design specification in
   [docs/project/design](docs/project/design/index.md) before working on a
   subsystem. The specification is the source of truth for intended design.

3. Read the architecture decision records in
   [docs/project/adr](docs/project/adr/index.md) that touch the area you are
   working on.

4. Use [scratch/](scratch/) for temporary working files: reference material
   you are given, plans you are asked to write, intermediate outputs. The
   directory exists in the repository but its contents are not part of it
   (`scratch/.gitignore` ignores everything except itself). Never reference
   files in `scratch/` by name from code or documentation that will be
   committed, because they come and go.

## Writing style, for every file in the repository

- Do not use em dashes in any file. Rephrase with commas, parentheses, colons
  or separate sentences. CI rejects the em dash character (U+2014) in tracked
  text files.

- In bulleted lists where items run to more than one line, put a blank line
  between the bullets, in docstrings, Markdown files, code comments and any
  other prose. This is about the raw file being readable. Be consistent
  within a list: if one item needs the spacing, space every item in it.

## Python code (the muru package, tests, fixtures, scripts)

- Always use type hints: on all function and method parameters and return
  types, and on attributes and variables where the type is not obvious from
  the assignment. When modifying code that lacks them, add them. mypy runs in
  strict mode on the muru package.

- Every function, method or property in the public API has a docstring saying
  what it does, except trivially obvious cases such as an accessor property
  named for the attribute it returns, and dunder methods implementing standard
  protocols.

- Put a blank line between a function or method docstring and the first line
  of code.

- Format and lint with ruff (`just lint` runs it). Do not hand-format.

## Code layout, for Python and Rust alike

- Write function bodies in paragraphs: group the statements that together
  perform one step, and separate the groups with blank lines. Natural
  boundaries include setup versus main work versus result, before and after a
  conditional or loop, and around `with`, `try`, `match` or `unsafe` blocks.
  Neither cram a body into one block nor put a blank line between every
  statement.

- Where it helps, start a paragraph with a short comment saying what the step
  does or why it is needed. Prefer one comment per logical block over
  line-by-line commentary, and omit it when the code already says it plainly.
  Put a blank line between such a block comment and the code below it.

- In Rust, every public item in a library crate has a doc comment, and every
  `unsafe` block has a `SAFETY:` comment explaining why it is sound. No
  `unsafe` outside the small reviewed modules (fork, descriptor passing,
  shared memory, PyO3). Errors use thiserror in libraries and anyhow at the
  binary edge. Logging uses tracing.

- Every user-visible setting is defined once in `muru-config`, and every CLI
  command once with clap. Reference documentation is generated from those
  definitions, never written by hand.

## Git and review

- Commit messages never include a co-authored-by trailer or any other agent
  attribution. A commit message hook, installed by `just bootstrap`, rejects
  them.

- Never commit on your own initiative. Finish the piece of work, summarise it
  and wait to be told to commit. Permission to commit covers only the work it
  was given for and does not carry forward to later steps of a multi-step
  plan, each of which needs its own review and instruction. Uncommitted
  changes are how review happens, so committing early makes review harder.

- Work happens on feature branches merged into `develop`, and releases are
  made from `main` through a release pull request from `develop`. After
  merging a feature branch into `develop` and pushing, the work is not treated
  as landed until the GitHub Actions run for the pushed merge has passed
  (check with `gh run list --branch develop` and `gh run watch`). Only then
  report the change as on the remote and delete the feature branch. If CI
  fails, keep the feature branch, report the failure, and wait for
  instructions.

## Project process

- Run the relevant `just test` targets before reporting work as complete,
  and `just lint`. Say which targets you ran and whether they passed.

- The specification is the current intended design, never a frozen
  document. When the implementation needs to differ from it, propose an ADR
  (copy `docs/project/adr/template.md`) and update the specification page in
  the same change instead of deviating silently. Add new material as new
  subsections so section numbers stay stable.

- Keep the lists in sections 19 and 20 of the specification live. When a
  change resolves an open question (19.2), retires a risk (19.3) or brings a
  future idea (20) into scope, write the outcome into the relevant section
  (and the decisions table in 19.1 for a significant choice), record the
  reasoning in an ADR, and remove the entry from the list in the same change.
  The history of those lists is the git history and the ADR trail, not the
  lists themselves.

- ADRs are append-only. Never edit the decision in an existing record; add a
  new one that supersedes it and set the old record's status to superseded.

- Add a changelog fragment under `changelog.d/` for user-visible changes
  (see `changelog.d/README.md`), and keep generated references (CLI,
  configuration, API) up to date by running their generators.

- Definition of done for a feature: implementation, unit tests, an integration
  test, metrics and events where relevant, documentation, and a changelog
  fragment.

## Commands

| Command | Does |
| --- | --- |
| `just doctor` | Check prerequisites and report what is missing, with install commands |
| `just bootstrap` | Install Rust components, cargo tools, Python interpreters, the dev environment and the extension |
| `just dev` | Build the extension into the development virtualenv (maturin develop) |
| `just lint` | rustfmt, clippy, ruff, mypy and the repository text checks |
| `just test` | Rust unit tests and Python unit tests |
| `just test-integration` | Integration scenarios that need no Docker or network |
| `just conformance` | WSGI, ASGI, HTTP and WebSocket conformance suites |
| `just test-docker` | Container scenarios (needs a container runtime) |
| `just test-linux` | Linux suites inside the development container |
| `just docs` | Build the documentation with warnings as errors |
| `nox -s tests-3.13` | Python tests on one interpreter, created with uv |

## Test markers

Tests that need something unusual carry a pytest marker and are skipped by
default: `requires_docker`, `requires_network`, `requires_root`,
`requires_linux` and `slow`. Linux-only behaviour (cgroups, subreaper,
O_TMPFILE uploads, USS and PSS, several edge processes, inotify, systemd,
container releases) is tested in CI on every pull request and in the Linux
virtual machine (`just vm-up`, `just vm-test`), so it need not pass locally
on macOS, but it must not be skipped silently when it could have run.

## Where things are

| Path | Holds |
| --- | --- |
| `crates/` | Rust workspace crates (section 14.1 of the specification) |
| `python/muru/` | The runtime library's Python sources |
| `proto/` | Worker and admin protocol definitions |
| `packages/` | The two PyPI distributions, muructl and muru |
| `docs/` | Sphinx documentation, including the specification and ADRs |
| `tests/` | Python, integration and conformance tests and fixture applications |
| `changelog.d/` | towncrier changelog fragments |
| `scratch/` | Temporary working files, ignored by git |
