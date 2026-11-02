# Configuration

Every key of every configuration file Muru reads, with its type and what it
does. The pages are generated from the definitions `muructl` parses the
files with, so they list exactly the keys it accepts; an unknown key is
refused, with its line.

| File | Where it is | Page |
| --- | --- | --- |
| The daemon's configuration | `/etc/muru/muru.toml` for the system daemon; for a user's, `~/.config/muru/muru.toml` on Linux or `~/Library/Application Support/muru/muru.toml` on macOS (see [the daemon](../../howto/daemon.md)) | [Daemon and applied files](daemon.md) |
| Applied files | Any file `muructl apply` is given | [Daemon and applied files](daemon.md) |
| The project's settings | `[tool.muru]` in the project's `pyproject.toml`, or the same keys at the top of a `muru.toml` beside it | [Project](project.md) |
| A developer's own settings | `muru.local.toml` beside the project | [muru.local.toml](local.md) |
| `muructl serve` settings | The file `--config` or `MURU_SERVE_CONFIG` names | [muructl serve](serve.md) |

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
| The `muructl serve` file | `https://docs.muruhq.dev/en/latest/schema/serve.schema.json` |

A comment on a file's first line points Taplo at its schema:

```toml
#:schema https://docs.muruhq.dev/en/latest/schema/daemon.schema.json
```

```{toctree}
:hidden:

daemon
project
local
serve
```
