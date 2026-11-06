# Configuration

Every key of every configuration file Muru reads, with its type and what it
does. The pages are generated from the definitions `muructl` parses the
files with, so they list exactly the keys it accepts; an unknown key is
refused, with its line.

| File | Where it is | Page |
| --- | --- | --- |
| The daemon's configuration | `/etc/muru/muru.toml` for the system daemon; for a user's, `~/.config/muru/muru.toml` on Linux or `~/Library/Application Support/muru/muru.toml` on macOS (see [the daemon](../../howto/daemon.md)) | [Daemon and applied files](daemon.md) |
| Applied files | Any file `muructl apply` is given; `muru.daemon.toml` when it is kept beside a project's code | [Daemon and applied files](daemon.md) |
| The project's settings | `[tool.muru]` in the project's `pyproject.toml`, or the same keys at the top of a `muru.toml` beside it; or the file `muructl serve --config` names in their place | [Project](project.md) |
| A developer's own settings | `muru.local.toml` beside the project | [muru.local.toml](local.md) |

Two of these are called `muru.toml` and hold different keys. The
daemon's is in its configuration directory, and a project's is beside
its code. A file in the daemon's format that is kept beside a project's
code, such as one for `muructl apply`, is named `muru.daemon.toml`, so
it is never read as the project's settings.

A project with both a `muru.toml` and a `[tool.muru]` table in its
`pyproject.toml` uses the `muru.toml`. The table is not read and the two
are not merged, which `muructl run`, `muructl serve`, `muructl check`
and `muructl deploy` say when they find both.

A duration is a number of seconds, or a number with a unit, `ms`, `s`,
`m`, `h`, `d` or `y`: `250ms`, `1.5s`, `10m`. A size is a number of bytes,
or a number with `KB`, `MB` or `GB` (or `K`, `M` or `G`), which count in
powers of 1024: `512KB`, `10MB`. `NAME`, `ID` and `STATUS` in a table's path stand for the key you
choose: an application's name, a release's identifier, an HTTP status.

## JSON Schema

Each file has a JSON Schema, for an editor that checks TOML against one,
such as one using the Taplo language server:

| File | Schema |
| --- | --- |
| The daemon's configuration and applied files | `https://docs.muruhq.dev/en/latest/schema/daemon.schema.json` |
| `[tool.muru]` and `muru.toml` | `https://docs.muruhq.dev/en/latest/schema/project.schema.json` |
| `muru.local.toml` | `https://docs.muruhq.dev/en/latest/schema/local.schema.json` |

A comment on a file's first line points Taplo at its schema:

```toml
#:schema https://docs.muruhq.dev/en/latest/schema/daemon.schema.json
```

```{toctree}
:hidden:

daemon
project
local
```
