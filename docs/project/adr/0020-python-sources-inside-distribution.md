---
status: accepted
date: 2026-10-23
---

# ADR 0020: The runtime library's Python sources live inside the muru distribution directory

## Context and problem statement

The repository layout in specification section 14.1 put the runtime
library's Python sources at `python/muru`, beside the Cargo workspace, with
`packages/muru/pyproject.toml` pointing at them through
`python-source = "../../python"`. The first release build produced muru
wheels containing only the compiled extension: maturin's wheel writer walks
only the directory holding `pyproject.toml` and keeps files under the Python
package, so a `python-source` outside that directory is never visited. The
sdist writer handles the same setting, which is why the sdist was complete
and the problem only showed in the wheel smoke test.

## Decision drivers

- Wheels must contain the Python sources, and the release pipeline must
  catch it if they do not.

- Avoid depending on a maturin behaviour that may or may not change.

## Considered options

- Keep `python/muru` and wait for or patch maturin.

- Symlink the directory into the package: maturin no longer follows
  symlinked directories.

- Move the sources to `packages/muru/python/muru`, with
  `python-source = "python"`.

## Decision outcome

Chosen option: move the sources. The Python package and the extension crate
it wraps are shipped by one distribution, and its directory is the natural
home for both the metadata and the sources. Specification section 14.1 is
updated to match, as are the test, type-checking and documentation
configuration that pointed at the old path.

### Consequences

- Good: wheels are complete, with no reliance on maturin resolving parent
  paths; the distribution directory is self-contained.

- Bad: the Python sources sit one level deeper than the crates, and
  contributors used to the specification's first layout need to look under
  `packages/muru`.

## More information

Specification sections 14.1 and 17.1. maturin 1.15.0,
`src/module_writer/mod.rs`, `write_python_part`.
