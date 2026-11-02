<!-- Generated from crates/muru-config by tests/reference.rs; do not edit. -->

# The project's [tool.muru] table

The keys an application ships with: the `[tool.muru]` table of its `pyproject.toml`, or the same keys at the top level of a `muru.toml` beside it. Every key is optional. A command line flag, or a release declared to the daemon, takes precedence over the key.

The JSON Schema of the same keys, for an editor that checks TOML against one, is `https://docs.muruhq.dev/en/latest/schema/project.schema.json`.

(config-project-tool-muru)=
## `[tool.muru]`

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `accel_redirect` | boolean | `false` | Treat an `X-Accel-Redirect` response header as an internal redirect, for the default mount and any mount that does not say otherwise. |
| `build` | table, {ref}`[tool.muru.build] <config-project-tool-muru-build>` |  | When and how the environment is built. |
| `bytecode` | string | `"redirect"` | Where compiled bytecode goes under the daemon. `redirect`: A writable directory of the release's own under the state directory, so nothing lands in the source tree (the default). `source`: `__pycache__` beside the source, as Python does by default; what a release with precompiled bytecode uses. `off`: No bytecode is written. |
| `cgi_local_redirect` | boolean | `false` | Treat a 200 response with a `Location` starting with `/` and no body as an internal redirect, the CGI convention. |
| `commands` | table of tables, {ref}`[tool.muru.commands.NAME] <config-project-tool-muru-commands-name>` |  | Commands the application declares, by name: run with the release on request, with inputs. |
| `concurrency` | integer | `1000` | Requests an ASGI worker's event loop takes at once. |
| `entrypoint` | string |  | The application as module:callable. |
| `env` | table of strings or tables, {ref}`[tool.muru.env.NAME] <config-project-tool-muru-env-name>` |  | Environment variables for the workers: a value, or a secret reference resolved when the release starts. |
| `error_override` | table, {ref}`[tool.muru.error_override] <config-project-tool-muru-error-override>` |  | Replace the application's own error responses with error pages. |
| `error_pages` | table of tables, {ref}`[tool.muru.error_pages.STATUS] <config-project-tool-muru-error-pages-status>` |  | Custom error pages, by status or status:reason. |
| `factory` | boolean | `false` | Whether the entrypoint is a factory returning the application. |
| `factory_phase` | string | `"worker"` | When a factory is called. `worker`: In each worker, after the post_fork hook. `host`: Once in the host, during preload. |
| `groups` | table of tables, {ref}`[tool.muru.groups.NAME] <config-project-tool-muru-groups-name>` |  | Worker groups by name. |
| `grpc_web_origins` | array of strings |  | The origins whose pages may call the gRPC methods through gRPC-Web, or `*`. |
| `handoff` | string |  | The interface the entrypoint's requests may be handed to. `asgi`: Continue the request as an ASGI application on the event loop. `wsgi`: Continue the request as a WSGI callable on the thread pool. |
| `hooks` | string |  | The module providing the lifecycle hooks. |
| `interface` | string | `"auto"` | The interface the entrypoint speaks. `auto`: Detect from the callable: a coroutine function or an object with an `async def __call__` is ASGI, anything else WSGI. `wsgi`: PEP 3333. `asgi`: ASGI 3. `grpc`: gRPC services: the entrypoint is a registration function given a server to add grpcio servicers to. |
| `logging` | table, {ref}`[tool.muru.logging] <config-project-tool-muru-logging>` |  | How Python logging is set up in the workers. |
| `maintenance` | table, {ref}`[tool.muru.maintenance] <config-project-tool-muru-maintenance>` |  | What maintenance mode lets through. |
| `max_lifetime` | string |  | How long a worker lives before it is replaced. |
| `max_requests` | integer |  | Requests a worker serves before it is replaced. |
| `memory` | table, {ref}`[tool.muru.memory] <config-project-tool-muru-memory>` |  | The memory policy for the workers. |
| `mounts` | table of tables, {ref}`[tool.muru.mounts.NAME] <config-project-tool-muru-mounts-name>` |  | Mounts by name. |
| `preload` | string | `"auto"` | Whether the application is imported once, in the host, before the workers are forked from it. `auto`: Preload when the import check passes, otherwise import per worker. `true`: Preload, and fail the release when the import check does not pass. `false`: Import in every worker. |
| `processes` | integer | `1`; under `muructl serve`, the container's CPU limit, else the CPU count | The default group's worker processes. |
| `python` | string | found at start | The interpreter: a path, or a request in uv's syntax such as "3.13", "3.14t" or ">=3.12". |
| `reflection` | boolean | `false` | Serve gRPC server reflection for the registered services. |
| `run` | table, {ref}`[tool.muru.run] <config-project-tool-muru-run>` |  | How `muructl run` serves the project, which the daemon ignores. |
| `scaling` | table, {ref}`[tool.muru.scaling] <config-project-tool-muru-scaling>` |  | How the default group is scaled; static when absent. |
| `sendfile_header` | string |  | A second response header honoured as `X-Muru-Sendfile` is, such as `X-Sendfile` for the frameworks that send that one. |
| `sendfile_roots` | array of strings |  | Directories a path in an `X-Muru-Sendfile` response header must lie in for the edge to serve the file, for the default mount and any mount that does not say otherwise; with none every hint is refused. |
| `serve` | table, {ref}`[tool.muru.serve] <config-project-tool-muru-serve>` |  | How `muructl serve` runs the project in a container, which the daemon and `muructl run` ignore. |
| `static` | array of tables, {ref}`[[tool.muru.static]] <config-project-tool-muru-static>` |  | The application's own static directories, served by the edge from the release's source, which is the project directory under `muructl run`. |
| `sync_threads` | integer | `8` | Threads for synchronous code called from an ASGI application. |
| `threads` | integer | `4` | Application threads per worker. |
| `timeouts` | table, {ref}`[tool.muru.timeouts] <config-project-tool-muru-timeouts>` |  | The timeouts the host enforces on its workers. |

(config-project-tool-muru-build)=
## `[tool.muru.build]`

The `[tool.muru.build]` table: when and how the release's environment is built from the lock. `when` is read by `muructl run` and the daemon alike; the rest describes the deployment flavour, which development ignores.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `compile_bytecode` | boolean | `true` | Compile bytecode at build time. |
| `extras` | array of strings |  | Extras the deployment environment includes. |
| `groups` | array of strings |  | Dependency groups beyond the defaults; dev is never included. |
| `source_builds` | boolean | `false` | Allow sdists to be built on the node. |
| `when` | string | `auto`; `never` under `muructl run --allow-public` | When an environment is built: `auto` builds a missing one and leaves an existing one alone, `always` builds on every start and on a lock change, `never` builds nothing. `muructl serve` never builds one. `auto`: Build a missing environment; leave an existing one alone at start, and under `muructl run` build when the lock changes. `always`: Build before every start too. `never`: Build nothing. |

(config-project-tool-muru-commands-name)=
## `[tool.muru.commands.NAME]`

A command an application declares, the `[tool.muru.commands.NAME]` or `[app.NAME.release.ID.commands.NAME]` table: a program to run, or a Python callable to call, with the release.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `call` | string |  | A callable as module:function, called with the inputs as keyword arguments, instead of `run`. |
| `description` | string |  | What the command does, for `muructl command list`. |
| `env` | table of strings |  | Environment variables for this command alone, over the release's, with `{name}` where an input's value goes. |
| `exclusive` | boolean | `false` | Run one at a time for the application: a run asked for while one is under way is refused. |
| `inputs` | table of tables, {ref}`[tool.muru.commands.NAME.inputs.NAME] <config-project-tool-muru-commands-name-inputs-name>` |  | The values the command takes, by name. |
| `run` | array of strings |  | The program and its arguments, run without a shell, with `{name}` where an input's value goes; `python` is the release's interpreter. |
| `timeout` | string |  | How long it may run before it is stopped, for example 10m; no limit when absent. |

(config-project-tool-muru-commands-name-inputs-name)=
## `[tool.muru.commands.NAME.inputs.NAME]`

An input a command takes.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `default` | string |  | The value when none is given. |
| `description` | string |  | What the value is, for `muructl command list` and prompts. |
| `pattern` | string |  | A regular expression the whole value must match. |
| `required` | boolean | `false` | The command refuses to run without it. |
| `secret` | boolean | `false` | The value is never shown, logged or put in an event, and may only be used in `env`. |

(config-project-tool-muru-env-name)=
## `[tool.muru.env.NAME]`

A secret reference in an `[env]` table.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `as` | string | `"env"` | How the value is delivered: `env` as the variable, or `file` as a file only the release's user can read, whose path is in `NAME_FILE`. |
| `secret` | table, {ref}`[tool.muru.env.NAME.secret] <config-project-tool-muru-env-name-secret>` | required | The store and key. |

(config-project-tool-muru-env-name-secret)=
## `[tool.muru.env.NAME.secret]`

Which secret: a store declared in the daemon's file, and a key in it.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `key` | string | required | The key within the store. |
| `store` | string | required | The store's name. |

(config-project-tool-muru-error-override)=
## `[tool.muru.error_override]`

Replacing an application's own error responses with error pages, the `error_override` table.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `statuses` | array of integers |  | The statuses whose responses are replaced. |
| `when` | string | `"all"` | When a listed response is replaced: `all` (the default), `empty_body`, `not_json` or `html_only`. |

(config-project-tool-muru-error-pages-status)=
## `[tool.muru.error_pages.STATUS]`

A custom error page: one entry of an `error_pages` table, keyed by status (`404`) or status and reason (`"503:maintenance"`). Exactly one of `file`, `template` and `path` is given.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `content_type` | string | `"text/html; charset=utf-8"` | The content type of a template. |
| `file` | string |  | A file sent as the body, with the content type its name implies. A release's page may be a path relative to its source directory; elsewhere the path is absolute. |
| `path` | string |  | An application path the edge asks for the page, as an internal sub-request that sees `REDIRECT_STATUS` and `muru.error_reason`. |
| `template` | string |  | A template the edge renders, with `{status}`, `{title}`, `{reason}`, `{description}`, `{request_id}`, `{retry_after}`, `{host}` and `{path}` filled in. |

(config-project-tool-muru-groups-name)=
## `[tool.muru.groups.NAME]`

A worker group, the `[app.NAME.release.ID.groups.NAME]` or `[tool.muru.groups.NAME]` table: the paths it serves and its own sizes. Paths not claimed by a group go to the default group, which is the release's own settings.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `concurrency` | integer | the release's | Requests an ASGI worker's event loop takes at once. |
| `match` | table, {ref}`[tool.muru.groups.NAME.match] <config-project-tool-muru-groups-name-match>` |  | What the group asks of a request beside its path: the client classes, hosts and methods it takes, each list any of its entries; a group asking more comes before one asking less, and a group with lists and no `paths` takes every path. |
| `memory` | table, {ref}`[tool.muru.memory] <config-project-tool-muru-memory>` |  | The group's own memory policy, each key the release's when absent. |
| `mounts` | array of strings |  | The named mounts the group's workers serve, beside the default mount, which every worker serves; every mount when absent. The group's paths must lie within them. |
| `optional` | boolean | `false` | Whether the release is ready without the group's workers: its requests wait until one listens. |
| `paths` | array of strings |  | Path prefixes, with an optional trailing `*`; the most specific match across all groups wins. |
| `processes` | integer | the release's, or `scaling.min_workers` when higher | The group's worker processes. |
| `scaling` | table, {ref}`[tool.muru.scaling] <config-project-tool-muru-scaling>` |  | How the group is scaled; static when absent. |
| `sync_threads` | integer | the release's | Threads for synchronous code called from an ASGI application. |
| `threads` | integer | the release's | Application threads per worker. |
| `timeouts` | table, {ref}`[tool.muru.timeouts] <config-project-tool-muru-timeouts>` |  | The group's own timeouts, each the release's when absent; `shutdown_timeout` and `websocket_drain_grace` belong to the release. |

(config-project-tool-muru-groups-name-match)=
## `[tool.muru.groups.NAME.match]`

The requests a worker group takes for its paths: every list given must name the request's class, host or method.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `client_class` | array of strings |  | Client classes. |
| `host` | array of strings |  | Hosts: a virtual host's name or a name it answers to, or under `muructl run` the name the request carries. |
| `method` | array of strings |  | Methods, compared in upper case. |

(config-project-tool-muru-logging)=
## `[tool.muru.logging]`

The `[logging]` table: how Python logging is set up in the workers.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `python` | table, {ref}`[tool.muru.logging.python] <config-project-tool-muru-logging-python>` |  | Python logging defaults. |

(config-project-tool-muru-logging-python)=
## `[tool.muru.logging.python]`

The `[logging.python]` table: what loggers without handlers of their own do, set before the application is imported.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `capture_warnings` | boolean | `false` | Whether warnings.warn goes through the py.warnings logger. |
| `format` | string | `standard`; `json` under `muructl serve --log-format json` | The format of the platform handler: standard, json, or a logging format string. |
| `level` | string | `"WARNING"` | The root logger's level. `DEBUG`: Everything. `INFO`: Informational messages and above. `WARNING`: Python's default: warnings and above. `ERROR`: Errors and above. `CRITICAL`: Only critical messages. |
| `loggers` | table of strings |  | Levels for named loggers. |

(config-project-tool-muru-maintenance)=
## `[tool.muru.maintenance]`

What maintenance mode lets through, the `maintenance` table of a release.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `bypass_cookie` | string |  | A `name=value` cookie that lets a client through. |
| `exempt_addresses` | array of strings |  | Client addresses, or ranges in CIDR form, served as usual. |
| `exempt_paths` | array of strings |  | Path prefixes served as usual while the application is in maintenance. |
| `retry_after` | string | `"60s"` | The `Retry-After` the maintenance page carries, for example 5m. |

(config-project-tool-muru-memory)=
## `[tool.muru.memory]`

The `[memory]` table of a release or a `[tool.muru]` table: the limits a worker's memory is held to.

The same keys apply at `[tool.muru.groups.NAME.memory]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `check_interval` | string | `"5s"` | How often memory is measured. |
| `hard_limit` | string | off; under `muructl serve` with a memory limit, a share of it | Over this the worker leaves routing at once and is replaced, for example 1200MB. |
| `measure` | string | `"uss"` | What is measured: uss (the default), pss or rss. `uss`: Unique set size: pages no other process shares, the default, since preloaded pages are shared copy-on-write. `pss`: Proportional set size: shared pages counted in proportion. `rss`: Resident set size: every resident page. |
| `reclaim_first` | boolean | `true` | Whether to collect garbage and trim the heap before recycling over the soft limit. |
| `soft_for` | string | `"2m"` | How long the worker may stay over the soft limit. |
| `soft_limit` | string |  | Over this for soft_for, the worker reclaims and then recycles, for example 800MB; off when absent. |

(config-project-tool-muru-mounts-name)=
## `[tool.muru.mounts.NAME]`

A mount, the `[app.NAME.release.ID.mounts.NAME]` or `[tool.muru.mounts.NAME]` table: a path served by another entrypoint in the same workers. The release's own entrypoint is the default mount at /.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `accel_redirect` | boolean | the release's | Treat an `X-Accel-Redirect` response header from the mount as an internal redirect. |
| `accept_redirects_from` | array of strings |  | Other applications whose internal redirects may reach the mount when it is internal; the release's own always may. |
| `cgi_local_redirect` | boolean | the release's | Treat a 200 response with a `Location` starting with `/` and no body as an internal redirect, the CGI convention. |
| `concurrency` | integer |  | Requests an ASGI mount runs at once on each worker, slots of its own at the edge; it shares the worker's slots when absent. |
| `entrypoint` | string | required | The application as module:callable. |
| `handoff` | string |  | The interface the mount's requests may be handed to; none when absent. `asgi`: Continue the request as an ASGI application on the event loop. `wsgi`: Continue the request as a WSGI callable on the thread pool. |
| `interface` | string | `"auto"` | The interface, when declared rather than detected. `auto`: Detect from the callable: a coroutine function or an object with an `async def __call__` is ASGI, anything else WSGI. `wsgi`: PEP 3333. `asgi`: ASGI 3. `grpc`: gRPC services: the entrypoint is a registration function given a server to add grpcio servicers to. |
| `internal` | boolean | `false` | Reachable only through internal redirects: a client's request for its path is answered 404. |
| `max_connections` | integer |  | Open WebSockets an ASGI mount holds at once on each worker, apart from the requests it runs; unbounded but by its slots when absent. |
| `path` | string |  | The path prefix; the longest wins, on segment boundaries. A gRPC mount has none: its calls are matched by service name. |
| `sendfile_header` | string | the release's | A second response header honoured as `X-Muru-Sendfile` is, such as `X-Sendfile`. |
| `sendfile_roots` | array of strings | the release's | Directories a path in an `X-Muru-Sendfile` response header must lie in for the edge to serve the file; the release's when absent, and an empty list refuses every hint. |
| `threads` | integer or table, {ref}`[tool.muru.mounts.NAME.threads] <config-project-tool-muru-mounts-name-threads>` |  | Threads of the mount's own, for a WSGI or gRPC mount; it shares the worker's pool when absent. |

(config-project-tool-muru-mounts-name-threads)=
## `[tool.muru.mounts.NAME.threads]`

The table form of a mount's threads.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `max` | integer | required | The most requests the mount runs at once, on its reserved threads and borrowed ones together. |
| `reserved` | integer | required | Threads only the mount uses. |

(config-project-tool-muru-run)=
## `[tool.muru.run]`

The `[tool.muru.run]` table of a pyproject.toml, or `[run]` in a muru.toml and in muru.local.toml: how `muructl run` serves this project, with a key for each flag of the command that describes the serving rather than the application. The daemon ignores it. Lists add across the layers, as a list flag adds to a key.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `access_log` | string | `"terminal"` | Where the access log goes: terminal, off, errors or a file path. |
| `access_log_exclude` | array of strings |  | Paths left out of the access log. |
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

(config-project-tool-muru-scaling)=
## `[tool.muru.scaling]`

The `scaling` table of a release, a worker group or a `[tool.muru]` table: the mode, the bounds, and the windows and budgets that keep the autoscaler from churning. The release's own table is the default group's.

The same keys apply at `[tool.muru.groups.NAME.scaling]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `advisory` | boolean | `false` | Decide and report without acting. |
| `churn_budget` | integer | `6` | Reaps allowed per hour per release before scale-in is suspended. |
| `hold_after_growth` | string | `"5m"` | No scale-in this long after a scale-out. |
| `host_idle_timeout` | string |  | How long the release's host stays once every group has scaled to zero, after which it stops too and the next request starts it again; it stays while the release runs when omitted. In the release's own scaling table. |
| `idle_reap_after` | string | `"10m"` | A group wholly idle this long scales in without waiting for the scale-in window. |
| `max_spare` | integer | the larger of 3 and a quarter of `max_workers` | Spare mode: scale in while more idle workers than this remain. |
| `max_step_up` | integer | `4` | The most workers one scale-out adds. |
| `max_workers` | integer |  | The most workers; required by the spare and target modes. |
| `min_lifetime` | string | the larger of 15m and 30 times the time to ready | No worker younger than this is reaped. |
| `min_spare` | integer | `1` | Spare mode: scale out when fewer idle workers than this remain. |
| `min_workers` | integer | `1` | The fewest workers. |
| `mode` | string | `"static"` | How the group is scaled. `static`: A fixed worker count, the default. `spare`: Keep between min_spare and max_spare idle workers, Apache-style. `target`: Track a slot utilization and a queue wait target. |
| `queue_wait_p95` | string |  | Target mode: scale out while the p95 queue wait is over this, for example 100ms; off when absent. |
| `reap_interval` | string | `"60s"` | At most one worker reaped per this interval. |
| `scale_in_window` | string | `"10m"` | How long every scale-in condition must hold. |
| `scale_out_window` | string | `"5s"` | How long a scale-out condition must hold. |
| `slot_utilization` | number | `0.7` | Target mode: the share of slots to keep busy, between 0 and 1. |

(config-project-tool-muru-serve)=
## `[tool.muru.serve]`

The `[tool.muru.serve]` table (`[serve]` in muru.toml), and the file `MURU_SERVE_CONFIG` names: how `muructl serve` runs the application in a container. A key is a flag of the command in snake case, taking the flag's value; a list key takes the values the flag would be repeated with.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `access_log` | string | `"on"` | The access log: `on` (the default), `errors` or `off`. |
| `access_log_exclude` | array of strings |  | Paths left out of the access log. |
| `admin_listen` | string | `"0.0.0.0:9090"` | Where the probes and metrics are served, IP:PORT or `off`. |
| `admin_status` | boolean | `false` | Whether `/status` is served on the admin port. |
| `compression` | boolean | `true` | Whether responses are compressed. |
| `concurrency` | integer | `1000` | Requests an ASGI worker's event loop takes at once. |
| `entrypoint` | string |  | The application as module:callable. |
| `factory` | boolean | `false` | Whether the entrypoint is a factory. |
| `hooks` | string |  | The module providing the lifecycle hooks. |
| `interface` | string | `"auto"` | The interface the entrypoint speaks. |
| `listen` | string | `"0.0.0.0:8080"` | Where the application is served, IP:PORT. |
| `log_format` | string | `"text"` | The lines on standard output: `text` (the default), or `json` for a log collector. |
| `max_body_size` | string | `"10MB"` | The largest request body accepted. |
| `max_connections` | integer | `65536` | Connections open at once. |
| `max_connections_per_client` | integer | `256` | Connections one client may have open. |
| `max_lifetime` | string |  | How long a worker lives before it is replaced. |
| `max_requests` | integer |  | Requests a worker serves before it is replaced. |
| `max_requests_per_connection` | integer | `0` | Requests one connection may carry. |
| `memory` | array of strings |  | The memory policy as KEY=VALUE. |
| `min_ready` | integer | `1` | The workers ready in every worker group for `/readyz` to pass. |
| `preload` | string | `"auto"` | Whether the application is imported before the workers are forked: `auto`, `true` or `false`. |
| `processes` | integer or string | `"auto"` | Worker processes, or `auto` for the container's CPU limit, else the CPU count. |
| `python` | string | found at start | The interpreter: a path, or a request in uv's syntax. When absent, `/app/.venv` if there is no `VIRTUAL_ENV` and no project `.venv`, else the interpreter `muructl run` would find. |
| `run_dir` | string | `"/run/muru"` | Where everything written at run time goes. |
| `runtime_dir` | string | `"/opt/muru"` | Where the image holds the runtime library. |
| `shutdown_budget` | string | `"8s"` | How long the whole stop may take, the delay included. |
| `shutdown_delay` | string | `"0s"` | How long the listener keeps accepting after SIGTERM, readiness failing, with every connection closing after its response. |
| `slow` | string | `"1s"` | How slow a request is to count as an error for `access_log = "errors"`. |
| `spool_quota` | string | `"1GB"` | The most request bodies spooled to disk at once. |
| `static` | array of strings |  | Directories of files as PREFIX=DIR. |
| `sync_threads` | integer | `8` | Threads for synchronous code called from an ASGI application. |
| `threads` | integer | `4` | Threads per WSGI worker. |
| `timeout` | array of strings |  | Timeouts as NAME=DURATION, as `--timeout` takes them. |
| `timestamps` | boolean | `false` | Whether every line carries the full UTC time. |
| `trusted_proxy` | array of strings |  | The proxies whose forwarding headers are applied: addresses, CIDR ranges, or loopback, private and unix. |

(config-project-tool-muru-static)=
## `[[tool.muru.static]]`

A directory of an application's own files, an entry of the `[[tool.muru.static]]` or `[[app.NAME.release.ID.static]]` array: a path prefix the edge serves from a directory under the release's source, with the application taking what no file answers. Served by `muructl run` and the daemon alike, from the release the request was routed to.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `cache` | table, {ref}`[tool.muru.static.cache] <config-project-tool-muru-static-cache>` |  | The cache headers files are sent with. |
| `case` | string | `"strict"` | `strict` (the default), which matches names as they are on disk, or `insensitive`. |
| `dir` | string | required | The directory, relative to the source directory, or absolute. |
| `fallback` | string |  | The file served for paths with no file of their own, for a single-page application under the prefix. |
| `index` | array of strings | `["index.html"]` | The files tried, in order, when a directory is requested. |
| `path` | string | required | The path prefix, such as `/static/`; the longest wins, on segment boundaries, and `/` is not allowed, since every path would then be looked up as a file first. |
| `redirect_slash` | boolean | `true` | Whether a directory asked for without a trailing slash is redirected to the slash form. |

(config-project-tool-muru-static-cache)=
## `[tool.muru.static.cache]`

A static route's cache headers.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `except` | array of strings |  | Files that get no-cache instead, by pattern on their path within the route. |
| `immutable` | boolean | `false` | Whether files are marked immutable. |
| `max_age` | string |  | How long clients may keep a file, for example 1y or 1h. |

(config-project-tool-muru-timeouts)=
## `[tool.muru.timeouts]`

The `[timeouts]` table of a release or a `[tool.muru]` table: the timeouts the host enforces on its workers. The edge's own timeouts are the daemon's `[limits]`.

The same keys apply at `[tool.muru.groups.NAME.timeouts]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `gil_stall_timeout` | string | `"30s"` | How long a worker may go with no thread running Python, or with no report to its host, before it is replaced as for the hard timeout. |
| `hard_drain_grace` | string | `"10s"` | How long a worker being replaced is given to finish its other requests before it is told to stop. |
| `hard_timeout` | string |  | How long a request may run in a worker before the worker is replaced, for example 60s; off when absent. Must exceed the soft timeout by at least 5s. |
| `loop_stall_timeout` | string | `"10s"` | How long an ASGI worker's event loop may go without running before the worker is replaced as for the hard timeout. |
| `shutdown_timeout` | string | `"5s"` | How long a worker told to stop is given before it is killed, counted from when its WebSockets are closed. |
| `soft_timeout` | string |  | How long a request may run in a worker before it is interrupted by raising `muru.RequestTimeout` in its thread, for example 30s; off when absent. |
| `websocket_drain_grace` | string | `"0s"` | How long a worker told to stop keeps its open WebSockets before it closes them with 1001. |
