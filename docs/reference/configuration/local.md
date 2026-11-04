<!-- Generated from crates/muru-config by tests/reference.rs; do not edit. -->

# muru.local.toml

A developer's own settings for `muructl run`, in `muru.local.toml` beside the project, which is not committed. Its `[run]` table holds the keys of `[tool.muru.run]` and takes precedence over them; lists add to the project's.

The JSON Schema of the same keys, for an editor that checks TOML against one, is `https://docs.muruhq.dev/en/latest/schema/local.schema.json`.

(config-local)=
## Top level

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `build` | table, {ref}`[build] <config-local-build>` |  | When `muructl run` builds the environment. |
| `run` | table, {ref}`[run] <config-local-run>` |  | How `muructl run` serves the project, over `[tool.muru.run]`. |

(config-local-build)=
## `[build]`

The `[build]` table of muru.local.toml.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `when` | string |  | When the environment is built before the application starts. `auto`: Build a missing environment; leave an existing one alone at start, and under `muructl run` build when the lock changes. `always`: Build before every start too. `never`: Build nothing. |

(config-local-run)=
## `[run]`

The `[tool.muru.run]` table of a pyproject.toml, or `[run]` in a muru.toml and in muru.local.toml: how `muructl run` serves this project, with a key for each flag of the command that describes the serving rather than the application. The daemon ignores it. Lists add across the layers, as a list flag adds to a key.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `access_log` | string | `"on"` | Which requests the access log writes: `on` (the default), `errors` or `off`. |
| `access_log_exclude` | array of strings |  | Paths left out of the access log. |
| `access_log_file` | string |  | A file the access log's JSON lines go to in place of the terminal, relative to the project. |
| `compression` | boolean | `true` | Whether responses are compressed; `false` is `--no-compression`. |
| `dashboard` | string | `127.0.0.1:8081`; off under `--allow-public` | The dashboard's address, or `off`. |
| `hosts` | array of strings |  | Names the development certificate covers besides localhost. |
| `https` | boolean | `false` | Serve HTTPS beside HTTP. |
| `https_only` | boolean | `false` | Serve HTTPS alone, HTTP redirecting to it. |
| `https_port` | integer or string | `8443`, or `auto` when `port` is | The HTTPS port, or `auto`. |
| `port` | integer or string | `8080` | The HTTP port, or `auto`. |
| `reload` | boolean | `true` | Reload on changes. |
| `reload_dirs` | array of strings |  | Directories watched, relative to the project; the project when empty. |
| `reload_exclude` | array of strings |  | Patterns that never trigger a reload. |
| `reload_include` | array of strings |  | Patterns watched besides `**/*.py`. |
| `slow` | string | `"1s"` | How long a request may take before `errors` shows it. |
| `timestamps` | boolean | `false` | Whether every line on the terminal carries the full UTC time. |
| `tls_cert` | string |  | A certificate chain to serve, relative to the project. |
| `tls_key` | string |  | Its private key, relative to the project. |
