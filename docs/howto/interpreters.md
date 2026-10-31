# Choose the Python interpreter

Muru runs your application on an interpreter you name, on the environment
your project already has, or on a Python it finds for you through uv.
This page covers the first two and how a version request is resolved;
building the environment itself is the next step.

## What is used, in order

`muructl run` and `muructl deploy` settle on an interpreter the same way:

1. **A path.** `--python .venv/bin/python` or `--python /usr/bin/python3`
   is used as it is. A relative path is taken from the project.

2. **A request on the command line.** `--python 3.13` names a version,
   whatever environment exists, and uv finds it.

3. **An environment that exists.** Under `muructl run`, the active
   virtualenv (`VIRTUAL_ENV`) and then the project's own `.venv`; under
   the daemon, the source directory's `.venv`.

4. **The project's own request.** `python` in `[tool.muru]`, then a
   `.python-version` file, then `requires-python` in `[project]`, which
   is a constraint and chooses only when nothing else does.

5. **What was there before.** Under the daemon, the previous release's
   interpreter; then `python3` on `PATH`, with a line saying no
   environment was found.

A request is in uv's syntax: `3.13`, `3.14t` for a free-threaded build,
`>=3.12`, or `cpython@3.13`. uv looks at every Python on the machine,
including the ones it manages, and `muructl run` says what it chose:

```
muru: Python 3.13.9 (cp313) at /Users/me/.local/share/uv/python/cpython-3.13.9-macos-aarch64-none/bin/python3.13 (3.13 from .python-version)
```

## What is checked before anything starts

Every interpreter is run once, before the host, to learn its version,
whether the GIL is disabled, its platform and whether it has the `muru`
library. Two things fail a run or a deploy here rather than later:

- An interpreter Muru does not support, which is anything below CPython
  3.11, or PyPy.

- An interpreter outside the project's `requires-python`. A `.venv` built
  on 3.11 for a project that now says `requires-python = ">=3.12"` is
  refused with a message naming both versions and saying how to rebuild
  it, which catches the environment a developer forgot to rebuild after
  raising the requirement. A `--python 3.11` that contradicts the
  requirement is refused the same way.

## When the machine lacks the Python

A request names a Python the machine does not have, and uv finds nothing:

```
no Python matching 3.14 (from .python-version) was found on this machine; `uv python install 3.14` adds one
```

Under `muructl run` nothing is downloaded by muructl itself: run the
command the message names, or let the environment build download it, as
`uv sync` does for a project that requires it. The daemon downloads a
missing interpreter into its own directory when it builds the
environment, unless its `[tools]` table says `python_downloads =
"never"` or `offline = true`.

Without uv, a path or an existing environment works as before, and a
request fails with a message saying uv is needed. Install uv, or set
`MURU_UV` to where it is.

## The runtime library

The `muru` package that runs inside your interpreter is Muru's, not your
application's, so it is not a dependency of your project. muructl puts
the copy matching its own version on the host's `PYTHONPATH`, one
directory per interpreter tag, and goes to the network last:

1. If the environment already has `muru` of muructl's version, it is
   used as it is and nothing is injected or fetched. An offline machine
   that has it installed needs no flag.

2. Else an unpacked copy already in the cache is used: your user cache
   under `muructl run`, the daemon's state directory under the daemon.

3. Else the wheel is fetched from PyPI, once per interpreter tag, with
   its published hash checked, or from a local directory of wheels named
   by `MURU_WHEEL_DIR` under `muructl run` or `wheel_dir` in the daemon's
   `[tools]` table.

4. If that fails and the environment has `muru` of another version, it is
   used with a warning naming both versions.

Set `MURU_OFFLINE=1` (uv's `UV_OFFLINE=1` works too) under `muructl run`,
or `offline = true` in the daemon's `[tools]` table, on a machine where the
attempt to connect is itself a problem; the fetch is then skipped. Where
nothing usable exists, the message names the two ways out: a wheel in the
wheel directory, or `pip install muru==VERSION` into the environment.

For an image, `muructl runtime install --python /app/.venv/bin/python
--target /opt/muru` unpacks the library during the build, and the
container puts `/opt/muru` first on `PYTHONPATH`.

## The daemon's `[tools]` table

```toml
[tools]
uv = "/usr/local/bin/uv"          # in place of the lookup: MURU_UV, beside muructl, PATH
uv_version = ">=0.9"              # what the daemon expects; an older uv is reported
python_downloads = "automatic"    # or "never"
python_install_dir = "/var/lib/muru/python"
offline = false                   # true: nothing is ever downloaded
wheel_dir = "/srv/wheels"         # wheels for the runtime library and builds when offline
```

The daemon says at start-up which uv it found, and `muructl release show`
prints what each release runs on:

```
python       /srv/shop/.venv/bin/python (3.13.9, cp313), requested 3.13 from .python-version
```
