---
status: accepted
date: 2026-10-31
---

# ADR 0037: The runtime library is provided by muructl, not listed by the application

## Context and problem statement

The `muru` runtime library runs inside the application's interpreter.
Section 8.3 had the daemon put a copy per interpreter tag on the host's
PYTHONPATH, but `muructl run` used whatever `muru` the project's
environment held, so every tutorial began with `uv add muru`, every
workshop pinned `muru==VERSION` beside `muructl==VERSION`, and the server
and its runtime could disagree on a version (specification sections 6.8
and 8.3).

## Decision drivers

- The runtime library is part of the server, not of the application: its
  version must match muructl's, and an application should run unchanged
  under another server.

- The library has no Python dependencies, so no resolver or pip is needed
  to install it.

- The first five minutes of a tutorial should not include installing a
  server library into the project.

## Considered options

- The application depends on `muru`, as today, and muructl checks the
  version.

- muructl provides the matching `muru` on the host's PYTHONPATH in every
  mode, fetching and unpacking the wheel itself.

## Decision outcome

Chosen option: the second. One directory per interpreter tag, holding the
unpacked wheel for muructl's own version: STATE/runtime/TAG under the
daemon, CACHE/runtime/TAG for `muructl run` and `muructl build`, and a
directory the image's build stage fills (`muructl build --runtime-dir`, or
`muructl runtime install` on its own) for containers. The host starts with
the directory first on PYTHONPATH. `import muru` works with nothing in
`pyproject.toml`.

Use what exists, and go to the network last. A `muru` of muructl's own
version already in the application's environment is used as it is, with
nothing injected and nothing fetched; then an existing runtime directory;
only then is the wheel fetched by muructl from PyPI, verifying the
published hash, or from a local wheel directory, announced first and with a
short timeout. If the fetch fails or is not allowed, a `muru` of another
version in the environment is used with a warning when it speaks a
supported protocol version, and the release fails only when nothing usable
exists. So an offline machine that has `muru` installed needs no flag;
`tools.offline`, `MURU_OFFLINE` or `UV_OFFLINE` exist for machines where
the attempt itself is the problem. A mismatched copy in the environment is
otherwise shadowed by the injected one, with the same warning. The fetch
works the same with or without uv.

### Consequences

- Good: the runtime always matches muructl; applications carry no server
  dependency; tutorials and workshops drop `uv add muru` and the pins;
  nothing is installed into a system Python or a read-only environment.

- Bad: a first run on a new interpreter tag fetches a wheel, announced,
  unless the environment already holds the matching version; an
  application that imports `muru` in its own tests needs the library in
  its dev dependencies or on PYTHONPATH there too.

## More information

Specification sections 6.8, 8.3, 8.6 and 13.8. The wheels themselves stay
version-specific, [ADR 0012](0012-version-specific-wheels.md); this record
changes only who installs them. Decided in issue #237 on 2026-10-31;
implemented by #127, with the workshops' follow-up in muru-workshops#13.
