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
   committed, because they come and go. It is not where pending work is
   tracked: that is GitHub issues (see Project process).

## Writing style, for every file in the repository

- Do not use em dashes in any file. Rephrase with commas, parentheses, colons
  or separate sentences. CI rejects the em dash character (U+2014) in tracked
  text files.

- In bulleted lists where items run to more than one line, put a blank line
  between the bullets, in docstrings, Markdown files, code comments and any
  other prose. This is about the raw file being readable. Be consistent
  within a list: if one item needs the spacing, space every item in it.

- Text users see (CLI help, messages, warnings, errors, events, dashboard
  pages) never names a section of the specification, an implementation
  phase or an ADR, which are internal and go stale. Say what the thing is
  instead. Code comments and docstrings may cite them.
  `scripts/check-user-text.py` checks string literals, and a muructl unit
  test checks the rendered help.

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

Code is read far more often than it is written, so function bodies are laid
out as paragraphs with vertical space between the steps, never as one dense
block. The rules below are mechanical on purpose, so that a checker can
enforce them; when in doubt the blank line goes in.

- A compound statement (`if`, `for`, `while`, `try`, `with`, `match`, `loop`,
  `unsafe`, or a `let` whose value is a `match` or `if`) that spans more than
  two lines has a blank line before it, unless it is the first statement of
  its enclosing block or sits directly below a block comment.

- The first statement after such a block ends (after the dedent in Python,
  after the closing brace in Rust) has a blank line before it. `else`,
  `elif`, `except`, `finally`, a closing bracket and a continued method
  chain are not statements for this purpose.

- A short guard of one or two lines (`if x is None: return`, or an `if let
  ... else { return }` on two lines) is exempt from both rules, so a run of
  simple statements and guards may stay together. Do not put a blank line
  between every statement.

- Setup for a loop or a conditional is a paragraph of its own. The line that
  computes a deadline, a buffer or a lock goes above a blank line, and the
  loop below it; a block comment above the pair ties them together.

- Where it helps, start a paragraph with a short comment saying what the step
  does or why it is needed. Prefer one comment per logical block over
  line-by-line commentary, and omit it when the code already says it plainly.
  A block comment has a blank line below it and, unless it opens a block, a
  blank line above it.

- Docstrings and doc comments are followed by a blank line before the first
  line of code.

This is what the rules produce. The dense form is what to avoid:

```python
    def terminate_all(self) -> None:
        """Stop every worker: SIGTERM, a grace period, then SIGKILL."""
        for pid in self.workers:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + SHUTDOWN_GRACE_SECONDS
        while self.workers and time.monotonic() < deadline:
            try:
                pid, _status = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                self.workers.clear()
                break
            if pid == 0:
                time.sleep(0.05)
                continue
            self.workers.pop(pid, None)
        for pid in list(self.workers):
            log(f"worker pid {pid} did not stop, killing")
            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass
        self.workers.clear()
```

The same function as the rules lay it out, one paragraph per step:

```python
    def terminate_all(self) -> None:
        """Stop every worker: SIGTERM, a grace period, then SIGKILL."""

        for pid in self.workers:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass

        # Reap whatever exits within the grace period.

        deadline = time.monotonic() + SHUTDOWN_GRACE_SECONDS

        while self.workers and time.monotonic() < deadline:
            try:
                pid, _status = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                self.workers.clear()
                break

            if pid == 0:
                time.sleep(0.05)
                continue

            self.workers.pop(pid, None)

        # Whatever is left is killed and reaped.

        for pid in list(self.workers):
            log(f"worker pid {pid} did not stop, killing")

            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass

        self.workers.clear()
```

The same applies to Rust: a `match` or `for` of more than two lines gets a
blank line before it and the statement after its closing brace gets one too.
Code produced through scripts, heredocs or patches is held to the same rules;
reread every new or changed body as a reader before reporting it.
`scripts/check-layout.py` enforces the rules on Python and Rust files, runs
from `just lint` and the pre-commit hook, and inserts the missing blank lines
with `--fix`; the formatters keep what it inserts.

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

- Feature branches are merged into `develop` with a merge commit (`gh pr
  merge --merge`), never squashed, so each commit on the branch survives
  with the issue it names, and the history shows one commit an issue. That
  is why a branch carries one commit per issue rather than fix-up commits:
  amend or roll fixes into the issue's commit before the pull request.
  Release pull requests from `develop` into `main` use a merge commit too,
  so `main` keeps the full history of each release and tags sit on a real
  merge point. Never use squash or rebase merges, which are disabled in the
  repository settings; do not rebase or force-push a branch that has been
  pushed, either.

## Project process

- Run the relevant `just test` targets before reporting work as complete,
  and `just lint`. Say which targets you ran and whether they passed.

- The specification is the current intended design, never a frozen
  document. When the implementation needs to differ from it, propose an ADR
  (copy `docs/project/adr/template.md`) and update the specification page in
  the same change instead of deviating silently. Add new material as new
  subsections so section numbers stay stable.

- Track pending work as GitHub issues, never in files. Every bug, planned
  piece of work, part of a phase still to come and future idea has an
  issue in this repository; a change a workshop needs is an issue in
  GrahamDumpleton/muru-workshops, marked as blocked by the Muru issue it
  waits on. When work is found or deferred, open an issue in the same
  step instead of noting it in `scratch/`, a code comment or a document.
  Write it with the headings and label of the matching issue form, or a
  plain structure for work no form fits, and do not hard-wrap its text,
  since GitHub shows every line break. A pull request that finishes an
  issue says `Closes #N`.

- Keep the [project board](https://github.com/users/GrahamDumpleton/projects/1)
  true to the work, since it is how the maintainer decides what comes next.
  Every open issue of both repositories is on it (issues in this
  repository are added automatically; add a workshops issue with
  `gh project item-add`), with Priority, Phase and Size set and Order
  where it ranks against its neighbours. The maintainer moves issues to
  Next; take the Next issue with the lowest Order, move it to In Progress
  when its branch is made, and let `Closes #N` move it to In Review and
  Done. When work changes an issue's size, priority or what blocks it,
  update the issue and its fields in the same step. `gh project field-list
  1 --owner GrahamDumpleton` gives the field and option ids that
  `gh project item-edit` needs.

- Keep the lists in sections 19 and 20 of the specification live. When a
  change resolves an open question (19.2), retires a risk (19.3) or brings a
  future idea (20) into scope, write the outcome into the relevant section
  (and the decisions table in 19.1 for a significant choice), record the
  reasoning in an ADR, and remove the entry from the list in the same change.
  The history of those lists is the git history and the ADR trail, not the
  lists themselves. Each future idea has an issue too: update or close it
  in the same change.

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
| `just test-integration` | Integration scenarios that need no Docker or network (builds muructl and the extension first) |
| `just conformance` | WSGI, ASGI, HTTP and WebSocket conformance suites |
| `just test-docker` | Container scenarios (needs a container runtime) |
| `just test-linux` | Linux suites inside the development container |
| `just bench` | Performance scenarios against other servers (see bench/README.md; Linux for real numbers) |
| `just docs` | Build the documentation with warnings as errors |
| `just docs-latex` | Check the LaTeX source for the PDF builds cleanly (the PDF itself is built on Read the Docs) |
| `just changelog-draft` | Preview the changelog the fragments would produce |
| `just changelog` | Build the changelog from the fragments, in the release pull request |
| `just bump X.Y.Z` | Set the version in Cargo.toml, the only place it is stated |
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
| `proto/` | Worker and admin protocol definitions |
| `packages/` | The two PyPI distributions; `packages/muru/python/muru/` holds the runtime library's Python sources |
| `docs/` | Sphinx documentation, including the specification and ADRs |
| `tests/` | Python, integration and conformance tests and fixture applications |
| `changelog.d/` | towncrier changelog fragments |
| `scratch/` | Temporary working files, ignored by git |
