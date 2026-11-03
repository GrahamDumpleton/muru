<!-- Generated from crates/muru-config by tests/reference.rs; do not edit. -->

# The daemon's configuration and applied files

The keys of the daemon's configuration file, `muru.toml` in its configuration directory, and of the files `muructl apply` reads, which share one format. Listeners, limits, the proxy, compression, secret stores, logs, shutdown, metrics, events, the dashboard, the overlay webhook, client classes and rate limits belong to the daemon's own file; an applied file that sets them is refused. Applications, their releases, hosts, routes, upstreams and overlays may be in either.

The JSON Schema of the same keys, for an editor that checks TOML against one, is `https://docs.muruhq.dev/en/latest/schema/daemon.schema.json`.

(config-daemon)=
## Top level

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `app` | table of tables, {ref}`[app.NAME] <config-daemon-app-name>` |  | Applications by name. |
| `client_class` | array of tables, {ref}`[[client_class]] <config-daemon-client-class>` |  | Client classification rules, in order, from the daemon's own file. |
| `compression` | table, {ref}`[compression] <config-daemon-compression>` |  | How responses are compressed. |
| `daemon` | table, {ref}`[daemon] <config-daemon-daemon>` |  | How the daemon itself runs. |
| `dashboard` | table, {ref}`[dashboard] <config-daemon-dashboard>` |  | The dashboard. |
| `error_override` | table, {ref}`[error_override] <config-daemon-error-override>` |  | Replace the application's own error responses with error pages. |
| `error_pages` | table of tables, {ref}`[error_pages.STATUS] <config-daemon-error-pages-status>` |  | Custom error pages, by status or status:reason. |
| `events` | table, {ref}`[events] <config-daemon-events>` |  | Event sinks. |
| `host` | table of tables, {ref}`[host.NAME] <config-daemon-host-name>` |  | Virtual hosts by name. |
| `limits` | table, {ref}`[limits] <config-daemon-limits>` |  | Limits in force everywhere. |
| `listener` | array of tables, {ref}`[[listener]] <config-daemon-listener>` |  | Listeners. |
| `logs` | table, {ref}`[logs] <config-daemon-logs>` |  | Log rotation. |
| `metrics` | table, {ref}`[metrics] <config-daemon-metrics>` |  | The metrics listener. |
| `overlay` | table of tables, {ref}`[overlay.NAME] <config-daemon-overlay-name>` |  | Overlays by name: partial configuration laid over the routes and applications while active. |
| `overlay_webhook` | table, {ref}`[overlay_webhook] <config-daemon-overlay-webhook>` |  | The overlays' external trigger. |
| `proxy` | table, {ref}`[proxy] <config-daemon-proxy>` |  | Trusted proxies and the headers taken from them. |
| `rate_limit` | table of tables, {ref}`[rate_limit.NAME] <config-daemon-rate-limit-name>` |  | Rate limits by name, from the daemon's own file. |
| `route` | array of tables, {ref}`[[route]] <config-daemon-route>` |  | Routes, in the order written. |
| `secrets` | table, {ref}`[secrets] <config-daemon-secrets>` |  | Secret stores. |
| `shutdown` | table, {ref}`[shutdown] <config-daemon-shutdown>` |  | What SIGTERM means to the daemon, and how a shutdown runs. |
| `tools` | table, {ref}`[tools] <config-daemon-tools>` |  | uv and interpreter installs. |
| `upstream` | table of tables, {ref}`[upstream.NAME] <config-daemon-upstream-name>` |  | HTTP upstreams by name. |

(config-daemon-app-name)=
## `[app.NAME]`

An application's table and its releases.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `group` | string | the user's primary group | The group, else the user's primary group. |
| `priority` | integer | `0` | Which applications' scale-ups the node's budget admits first, the highest first. |
| `release` | table of tables, {ref}`[app.NAME.release.ID] <config-daemon-app-name-release-id>` |  | Releases by identifier. |
| `schedule` | array of tables, {ref}`[[app.NAME.schedule]] <config-daemon-app-name-schedule>` |  | Scheduled capacity. |
| `umask` | string | the daemon's own umask | The file creation mask, in octal, for example 027. |
| `user` | string | the daemon's own user | The user the application's hosts and workers run as, when the daemon runs as root. |

(config-daemon-app-name-release-id)=
## `[app.NAME.release.ID]`

A release definition, the `[app.NAME.release.ID]` table: the keys a deploy takes. `muructl run --print-config` writes one so a laptop setup can become a declared release, and `muructl apply` reads them.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `accel_redirect` | boolean | `false` | Treat an `X-Accel-Redirect` response header as an internal redirect, for the default mount and any mount that does not say otherwise. |
| `bind` | string | `"unix"` | How an `http` release's instances bind: `unix` (the default) or `port`. `unix`: A Unix socket per instance under the release's run directory. `port`: A free loopback port per instance. |
| `build` | table, {ref}`[app.NAME.release.ID.build] <config-daemon-app-name-release-id-build>` |  | When and how the environment is built on the node. |
| `bytecode` | string | `"redirect"` | Where compiled bytecode goes. `redirect`: A writable directory of the release's own under the state directory, so nothing lands in the source tree (the default). `source`: `__pycache__` beside the source, as Python does by default; what a release with precompiled bytecode uses. `off`: No bytecode is written. |
| `cgi_local_redirect` | boolean | `false` | Treat a 200 response with a `Location` starting with `/` and no body as an internal redirect, the CGI convention. |
| `command` | array of strings |  | The command an `http` release runs, with `{bind}` where the server takes its address; without `{bind}` the profile adds the server's own flags. |
| `commands` | table of tables, {ref}`[app.NAME.release.ID.commands.NAME] <config-daemon-app-name-release-id-commands-name>` |  | Commands the application declares, by name: run with the release on request, with inputs. |
| `concurrency` | integer | `1000` | Requests an ASGI worker's event loop takes at once. |
| `cwd` | string | `source` | The directory an `http` release's command runs in; `source` when absent, and one of the two is required. |
| `entrypoint` | string | the source's `[tool.muru]` | The application as module:callable, else from the source's `[tool.muru]` table. |
| `env` | table of strings or tables, {ref}`[app.NAME.release.ID.env.NAME] <config-daemon-app-name-release-id-env-name>` |  | Environment variables for the workers: a value, or a secret reference resolved when the release starts. |
| `error_override` | table, {ref}`[error_override] <config-daemon-error-override>` |  | Replace the application's own error responses with error pages. |
| `error_pages` | table of tables, {ref}`[error_pages.STATUS] <config-daemon-error-pages-status>` |  | Custom error pages, by status or status:reason. |
| `factory` | boolean | `false` | Whether the entrypoint is a factory. |
| `factory_phase` | string | `"worker"` | When a factory is called. `worker`: In each worker, after the post_fork hook. `host`: Once in the host, during preload. |
| `groups` | table of tables, {ref}`[app.NAME.release.ID.groups.NAME] <config-daemon-app-name-release-id-groups-name>` |  | Worker groups by name. |
| `grpc_web_origins` | array of strings |  | The origins whose pages may call the release's gRPC methods through gRPC-Web, answered with CORS: scheme, host and port, or `*`; none when empty. |
| `handoff` | string |  | The interface the default mount's requests may be handed to; none when absent. `asgi`: Continue the request as an ASGI application on the event loop. `wsgi`: Continue the request as a WSGI callable on the thread pool. |
| `hooks` | string |  | The module holding the lifecycle hooks, as a Python module name. |
| `instances` | integer | `1` | How many instances of the command an `http` release runs. |
| `interface` | string | `"auto"` | The interface, when declared rather than detected. `auto`: Detect from the callable: a coroutine function or an object with an `async def __call__` is ASGI, anything else WSGI. `wsgi`: PEP 3333. `asgi`: ASGI 3. `grpc`: gRPC services: the entrypoint is a registration function given a server to add grpcio servicers to. |
| `kind` | string | `"workers"` | What the release runs: `workers` from an entrypoint, the default, or `http`, an HTTP server started from `command`. `workers`: Muru's own workers, from an entrypoint. `http`: An HTTP server Muru starts and supervises, from a command. |
| `logging` | table, {ref}`[app.NAME.release.ID.logging] <config-daemon-app-name-release-id-logging>` |  | How Python logging is set up in the workers. |
| `maintenance` | table, {ref}`[app.NAME.release.ID.maintenance] <config-daemon-app-name-release-id-maintenance>` |  | What maintenance mode lets through. |
| `max_lifetime` | string |  | How long a worker lives before it is replaced, for example 1h, with jitter; off when absent. |
| `max_requests` | integer |  | Requests a worker serves before it is replaced, with jitter; off when absent. |
| `memory` | table, {ref}`[app.NAME.release.ID.memory] <config-daemon-app-name-release-id-memory>` |  | The memory policy for the workers. |
| `mounts` | table of tables, {ref}`[app.NAME.release.ID.mounts.NAME] <config-daemon-app-name-release-id-mounts-name>` |  | Mounts by name. |
| `preload` | string | `"auto"` | Whether the application is imported once, in the host, before the workers are forked from it. `auto`: Preload when the import check passes, otherwise import per worker. `true`: Preload, and fail the release when the import check does not pass. `false`: Import in every worker. |
| `processes` | integer | `1` | The default group's worker processes. |
| `profile` | string |  | The server an `http` release runs: gunicorn, uvicorn, granian, mod_wsgi-express, uwsgi or generic. `gunicorn`: gunicorn: `--bind unix:PATH` or `--bind HOST:PORT`, stopped by TERM. `uvicorn`: uvicorn: `--uds PATH` or `--host HOST --port PORT`, stopped by TERM. `granian`: Granian: `--uds PATH` or `--host HOST --port PORT`, stopped by TERM. `mod_wsgi-express`: mod_wsgi-express: `--host HOST --port PORT`, stopped by TERM. `uwsgi`: uWSGI: `--http-socket PATH` or `--http-socket HOST:PORT`, with `--master` and a hook that makes TERM a graceful stop, which in uWSGI 2.0 would otherwise reload. `generic`: Any server: the command binds through `{bind}` and `stop_signal` says how it is stopped. |
| `python` | string | found at start | The interpreter: a path, or a request in uv's syntax. |
| `readiness` | table, {ref}`[app.NAME.release.ID.readiness] <config-daemon-app-name-release-id-readiness>` |  | How an `http` release's instances are known to be ready. |
| `reflection` | boolean | `false` | Serve gRPC server reflection for the services the release's gRPC mounts register (default false). |
| `scaling` | table, {ref}`[app.NAME.release.ID.scaling] <config-daemon-app-name-release-id-scaling>` |  | How the default group is scaled; static when absent. |
| `sendfile_header` | string |  | A second response header honoured as `X-Muru-Sendfile` is, such as `X-Sendfile` for the frameworks that send that one. |
| `sendfile_roots` | array of strings |  | Directories a path in an `X-Muru-Sendfile` response header must lie in for the edge to serve the file, for the default mount and any mount that does not say otherwise; with none every hint is refused. |
| `source` | string |  | The source directory. A relative path is taken from the file's directory. Required by `muructl apply` for a `workers` release. |
| `static` | array of tables, {ref}`[[app.NAME.release.ID.static]] <config-daemon-app-name-release-id-static>` |  | The application's own static directories, served by the edge from the release's source, which is the project directory under `muructl run`. |
| `stop_signal` | string | the profile's, `TERM` | The signal that stops an `http` release's instance gracefully: TERM, INT, QUIT, HUP, USR1, USR2 or WINCH. Required by the generic profile; the other profiles know their server's. |
| `sync_threads` | integer | `8` | Threads for synchronous code called from an ASGI application. |
| `threads` | integer | `4` | Application threads per worker. |
| `timeouts` | table, {ref}`[app.NAME.release.ID.timeouts] <config-daemon-app-name-release-id-timeouts>` |  | The timeouts the host enforces on its workers. |

(config-daemon-app-name-release-id-build)=
## `[app.NAME.release.ID.build]`

The `[tool.muru.build]` table: when and how the release's environment is built from the lock. `when` is read by `muructl run` and the daemon alike; the rest describes the deployment flavour, which development ignores.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `compile_bytecode` | boolean | `true` | Compile bytecode at build time. |
| `extras` | array of strings |  | Extras the deployment environment includes. |
| `groups` | array of strings |  | Dependency groups beyond the defaults; dev is never included. |
| `source_builds` | boolean | `false` | Allow sdists to be built on the node. |
| `when` | string | `auto`; `never` under `muructl run --allow-public` | When an environment is built: `auto` builds a missing one and leaves an existing one alone, `always` builds on every start and on a lock change, `never` builds nothing. `muructl serve` never builds one. `auto`: Build a missing environment; leave an existing one alone at start, and under `muructl run` build when the lock changes. `always`: Build before every start too. `never`: Build nothing. |

(config-daemon-app-name-release-id-commands-name)=
## `[app.NAME.release.ID.commands.NAME]`

A command an application declares, the `[tool.muru.commands.NAME]` or `[app.NAME.release.ID.commands.NAME]` table: a program to run, or a Python callable to call, with the release.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `call` | string |  | A callable as module:function, called with the inputs as keyword arguments, instead of `run`. |
| `description` | string |  | What the command does, for `muructl command list`. |
| `env` | table of strings |  | Environment variables for this command alone, over the release's, with `{name}` where an input's value goes. |
| `exclusive` | boolean | `false` | Run one at a time for the application: a run asked for while one is under way is refused. |
| `inputs` | table of tables, {ref}`[app.NAME.release.ID.commands.NAME.inputs.NAME] <config-daemon-app-name-release-id-commands-name-inputs-name>` |  | The values the command takes, by name. |
| `run` | array of strings |  | The program and its arguments, run without a shell, with `{name}` where an input's value goes; `python` is the release's interpreter. |
| `timeout` | string |  | How long it may run before it is stopped, for example 10m; no limit when absent. |

(config-daemon-app-name-release-id-commands-name-inputs-name)=
## `[app.NAME.release.ID.commands.NAME.inputs.NAME]`

An input a command takes.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `default` | string |  | The value when none is given. |
| `description` | string |  | What the value is, for `muructl command list` and prompts. |
| `pattern` | string |  | A regular expression the whole value must match. |
| `required` | boolean | `false` | The command refuses to run without it. |
| `secret` | boolean | `false` | The value is never shown, logged or put in an event, and may only be used in `env`. |

(config-daemon-app-name-release-id-env-name)=
## `[app.NAME.release.ID.env.NAME]`

A secret reference in an `[env]` table.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `as` | string | `"env"` | How the value is delivered: `env` as the variable, or `file` as a file only the release's user can read, whose path is in `NAME_FILE`. |
| `secret` | table, {ref}`[app.NAME.release.ID.env.NAME.secret] <config-daemon-app-name-release-id-env-name-secret>` | required | The store and key. |

(config-daemon-app-name-release-id-env-name-secret)=
## `[app.NAME.release.ID.env.NAME.secret]`

Which secret: a store declared in the daemon's file, and a key in it.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `key` | string | required | The key within the store. |
| `store` | string | required | The store's name. |

(config-daemon-app-name-release-id-groups-name)=
## `[app.NAME.release.ID.groups.NAME]`

A worker group, the `[app.NAME.release.ID.groups.NAME]` or `[tool.muru.groups.NAME]` table: the paths it serves and its own sizes. Paths not claimed by a group go to the default group, which is the release's own settings.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `concurrency` | integer | the release's | Requests an ASGI worker's event loop takes at once. |
| `match` | table, {ref}`[app.NAME.release.ID.groups.NAME.match] <config-daemon-app-name-release-id-groups-name-match>` |  | What the group asks of a request beside its path: the client classes, hosts and methods it takes, each list any of its entries; a group asking more comes before one asking less, and a group with lists and no `paths` takes every path. |
| `max_lifetime` | string | the release's | How long a worker of the group lives before it is replaced, for example 1h, with jitter. |
| `max_requests` | integer | the release's | Requests a worker of the group serves before it is replaced, with jitter. |
| `memory` | table, {ref}`[app.NAME.release.ID.memory] <config-daemon-app-name-release-id-memory>` |  | The group's own memory policy, each key the release's when absent. |
| `mounts` | array of strings |  | The named mounts the group's workers serve, beside the default mount, which every worker serves; every mount when absent. The group's paths must lie within them. |
| `optional` | boolean | `false` | Whether the release is ready without the group's workers: its requests wait until one listens. |
| `paths` | array of strings |  | Path prefixes, with an optional trailing `*`; the most specific match across all groups wins. |
| `processes` | integer | the release's, or `scaling.min_workers` when higher | The group's worker processes. |
| `scaling` | table, {ref}`[app.NAME.release.ID.scaling] <config-daemon-app-name-release-id-scaling>` |  | How the group is scaled; static when absent. |
| `sync_threads` | integer | the release's | Threads for synchronous code called from an ASGI application. |
| `threads` | integer | the release's | Application threads per worker. |
| `timeouts` | table, {ref}`[app.NAME.release.ID.timeouts] <config-daemon-app-name-release-id-timeouts>` |  | The group's own timeouts, each the release's when absent; `shutdown_timeout` and `websocket_drain_grace` belong to the release. |

(config-daemon-app-name-release-id-groups-name-match)=
## `[app.NAME.release.ID.groups.NAME.match]`

The requests a worker group takes for its paths: every list given must name the request's class, host or method.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `client_class` | array of strings |  | Client classes. |
| `host` | array of strings |  | Hosts: a virtual host's name or a name it answers to, or under `muructl run` the name the request carries. |
| `method` | array of strings |  | Methods, compared in upper case. |

(config-daemon-app-name-release-id-logging)=
## `[app.NAME.release.ID.logging]`

The `[logging]` table: how Python logging is set up in the workers.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `python` | table, {ref}`[app.NAME.release.ID.logging.python] <config-daemon-app-name-release-id-logging-python>` |  | Python logging defaults. |

(config-daemon-app-name-release-id-logging-python)=
## `[app.NAME.release.ID.logging.python]`

The `[logging.python]` table: what loggers without handlers of their own do, set before the application is imported.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `capture_warnings` | boolean | `false` | Whether warnings.warn goes through the py.warnings logger. |
| `format` | string | `standard`; `json` under `muructl serve --log-format json` | The format of the platform handler: standard, json, or a logging format string. |
| `level` | string | `"WARNING"` | The root logger's level. `DEBUG`: Everything. `INFO`: Informational messages and above. `WARNING`: Python's default: warnings and above. `ERROR`: Errors and above. `CRITICAL`: Only critical messages. |
| `loggers` | table of strings |  | Levels for named loggers. |

(config-daemon-app-name-release-id-maintenance)=
## `[app.NAME.release.ID.maintenance]`

What maintenance mode lets through, the `maintenance` table of a release.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `bypass_cookie` | string |  | A `name=value` cookie that lets a client through. |
| `exempt_addresses` | array of strings |  | Client addresses, or ranges in CIDR form, served as usual. |
| `exempt_paths` | array of strings |  | Path prefixes served as usual while the application is in maintenance. |
| `retry_after` | string | `"60s"` | The `Retry-After` the maintenance page carries, for example 5m. |

(config-daemon-app-name-release-id-memory)=
## `[app.NAME.release.ID.memory]`

The `[memory]` table of a release or a `[tool.muru]` table: the limits a worker's memory is held to.

The same keys apply at `[app.NAME.release.ID.groups.NAME.memory]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `check_interval` | string | `"5s"` | How often memory is measured. |
| `hard_limit` | string | off; under `muructl serve` with a memory limit, a share of it | Over this the worker leaves routing at once and is replaced, for example 1200MB. |
| `measure` | string | `"uss"` | What is measured: uss (the default), pss or rss. `uss`: Unique set size: pages no other process shares, the default, since preloaded pages are shared copy-on-write. `pss`: Proportional set size: shared pages counted in proportion. `rss`: Resident set size: every resident page. |
| `reclaim_first` | boolean | `true` | Whether to collect garbage and trim the heap before recycling over the soft limit. |
| `soft_for` | string | `"2m"` | How long the worker may stay over the soft limit. |
| `soft_limit` | string |  | Over this for soft_for, the worker reclaims and then recycles, for example 800MB; off when absent. |

(config-daemon-app-name-release-id-mounts-name)=
## `[app.NAME.release.ID.mounts.NAME]`

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
| `threads` | integer or table, {ref}`[app.NAME.release.ID.mounts.NAME.threads] <config-daemon-app-name-release-id-mounts-name-threads>` |  | Threads of the mount's own, for a WSGI or gRPC mount; it shares the worker's pool when absent. |

(config-daemon-app-name-release-id-mounts-name-threads)=
## `[app.NAME.release.ID.mounts.NAME.threads]`

The table form of a mount's threads.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `max` | integer | required | The most requests the mount runs at once, on its reserved threads and borrowed ones together. |
| `reserved` | integer | required | Threads only the mount uses. |

(config-daemon-app-name-release-id-readiness)=
## `[app.NAME.release.ID.readiness]`

The `readiness` table of a managed HTTP release: how an instance is known to be ready.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `path` | string |  | The path probed with a GET until it answers 2xx or 3xx; without it, any HTTP response makes the instance ready. |
| `timeout` | string | `"60s"` | How long the instances get to become ready. |

(config-daemon-app-name-release-id-scaling)=
## `[app.NAME.release.ID.scaling]`

The `scaling` table of a release, a worker group or a `[tool.muru]` table: the mode, the bounds, and the windows and budgets that keep the autoscaler from churning. The release's own table is the default group's.

The same keys apply at `[app.NAME.release.ID.groups.NAME.scaling]`.

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

(config-daemon-app-name-release-id-static)=
## `[[app.NAME.release.ID.static]]`

A directory of an application's own files, an entry of the `[[tool.muru.static]]` or `[[app.NAME.release.ID.static]]` array: a path prefix the edge serves from a directory under the release's source, with the application taking what no file answers. Served by `muructl run` and the daemon alike, from the release the request was routed to.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `cache` | table, {ref}`[route.cache] <config-daemon-route-cache>` |  | The cache headers files are sent with. |
| `case` | string | `"strict"` | `strict` (the default), which matches names as they are on disk, or `insensitive`. |
| `dir` | string | required | The directory, relative to the source directory, or absolute. |
| `fallback` | string |  | The file served for paths with no file of their own, for a single-page application under the prefix. |
| `index` | array of strings | `["index.html"]` | The files tried, in order, when a directory is requested. |
| `path` | string | required | The path prefix, such as `/static/`; the longest wins, on segment boundaries, and `/` is not allowed, since every path would then be looked up as a file first. |
| `redirect_slash` | boolean | `true` | Whether a directory asked for without a trailing slash is redirected to the slash form. |

(config-daemon-app-name-release-id-timeouts)=
## `[app.NAME.release.ID.timeouts]`

The `[timeouts]` table of a release or a `[tool.muru]` table: the timeouts the host enforces on its workers. The edge's own timeouts are the daemon's `[limits]`.

The same keys apply at `[app.NAME.release.ID.groups.NAME.timeouts]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `gil_stall_timeout` | string | `"30s"` | How long a worker may go with no thread running Python, or with no report to its host, before it is replaced as for the hard timeout. |
| `hard_drain_grace` | string | `"10s"` | How long a worker being replaced is given to finish its other requests before it is told to stop. |
| `hard_timeout` | string |  | How long a request may run in a worker before the worker is replaced, for example 60s; off when absent. Must exceed the soft timeout by at least 5s. |
| `loop_stall_timeout` | string | `"10s"` | How long an ASGI worker's event loop may go without running before the worker is replaced as for the hard timeout. |
| `shutdown_timeout` | string | `"5s"` | How long a worker told to stop is given before it is killed, counted from when its WebSockets are closed. |
| `soft_timeout` | string |  | How long a request may run in a worker before it is interrupted by raising `muru.RequestTimeout` in its thread, for example 30s; off when absent. |
| `websocket_drain_grace` | string | `"0s"` | How long a worker told to stop keeps its open WebSockets before it closes them with 1001. |

(config-daemon-app-name-schedule)=
## `[[app.NAME.schedule]]`

An `[[app.NAME.schedule]]` entry: capacity for a window, shorthand for an overlay named `APP.NAME` that sets only the floor, the ceiling, how scale-in behaves and a deploy freeze.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `cron` | string |  | A recurring window's start times. |
| `duration` | string |  | A recurring window's length. |
| `end` | string |  | A one-off window's end. |
| `freeze_deploys` | boolean | `false` | Refuse deploys and rollouts during the window. |
| `group` | string | `"default"` | The worker group; the default group without it. |
| `max_workers` | integer |  | The most workers during the window. |
| `min_workers` | integer |  | The fewest workers during the window. |
| `name` | string | required | The schedule's name, unique within the application. |
| `scale_in` | string | `"normal"` | `hold` keeps the group from scaling in during the window; `normal`, the default, leaves it to the autoscaler. |
| `start` | string |  | A one-off window's start, as for an overlay's schedule. |
| `timezone` | string | the daemon's system time zone | The time zone of local times. |
| `warm_up` | string |  | How long before each window the floor applies, such as 30m. |

(config-daemon-client-class)=
## `[[client_class]]`

A `[[client_class]]` rule. A request gets the class when every input the rule names matches, each input matching when any of its entries does; rules are tried in order and a request that matches none is class `default`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `address_list` | string |  | A file of addresses and networks, one a line, as for `user_agent_list`. |
| `addresses` | array of strings |  | Client addresses and networks, such as 203.0.113.0/24. |
| `cookies` | table of strings |  | Cookies by name, each value matched by a regular expression. |
| `headers` | table of strings |  | Request headers by name, each value matched by a regular expression: a CDN's verdict, say. |
| `ja4` | array of strings |  | JA4 fingerprints of the client's TLS ClientHello, or their beginnings ending in `*`. |
| `ja4_list` | string |  | A file of JA4 fingerprints, one a line, as for `user_agent_list`. |
| `name` | string | required | The class's name, as logs, metrics, rate limits and applications see it. |
| `user_agent` | array of strings |  | User-Agent substrings, compared without regard to case. |
| `user_agent_list` | string |  | A file of User-Agent substrings, one a line, relative to the configuration file's directory and read again when it changes. |
| `user_agent_regex` | array of strings |  | Regular expressions matched against the User-Agent header. |
| `verify` | string |  | `dns` confirms the client's address by reverse and forward lookup against `verify_domains`; until it is confirmed the rule does not match. |
| `verify_domains` | array of strings |  | The domains a verified name must be, or end in. |

(config-daemon-compression)=
## `[compression]`

The `[compression]` table: how the edge compresses responses. Every key is optional.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | boolean | `true` | Whether responses are compressed at all. |
| `encodings` | array of strings | `["br", "zstd", "gzip"]` | The encodings offered in preference order: br, zstd and gzip. An empty list is refused; `enabled = false` turns compression off. |
| `level` | string | `"default"` | How hard to compress: fast, default or best. |
| `min_size` | string | `"1KB"` | The smallest body worth compressing, for example 1KB; at least 1 byte. |
| `types` | array of strings | `["text/*", "application/json", "application/javascript", "application/xml", "image/svg+xml"]` | The content types compressed: exact types or a family as text/*. An empty list is refused. |

(config-daemon-daemon)=
## `[daemon]`

The `[daemon]` table of the daemon's file: how the daemon itself runs.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `budget` | table, {ref}`[daemon.budget] <config-daemon-daemon-budget>` |  | The node's budget across every application. |
| `debug_exec` | boolean | `false` | Allow `muructl debug exec`, which runs code in a worker; off unless set. |
| `user` | string | `"muru"` | The service user: whom a daemon that runs as root starts the edge, and the applications that name no user of their own, as. Never root. |

(config-daemon-daemon-budget)=
## `[daemon.budget]`

The `[daemon.budget]` table: what the releases on the node may hold between them. Each limit is off when omitted.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `cpu` | number |  | CPU cores the workers use, as measured. |
| `memory` | string |  | Memory for the hosts and workers, as a size such as 12GB. |
| `workers` | integer |  | Worker processes and managed server instances. |

(config-daemon-dashboard)=
## `[dashboard]`

The `[dashboard]` table: where the daemon serves its dashboard and JSON gateway, and whether it takes actions.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `actions` | boolean | `false` | Whether a person signed in with `muructl dashboard login` may act from the dashboard; read-only when false. |
| `listen` | string |  | The loopback address the dashboard is served on, such as 127.0.0.1:8090; not served when omitted. |

(config-daemon-error-override)=
## `[error_override]`

Replacing an application's own error responses with error pages, the `error_override` table.

The same keys apply at `[host.NAME.error_override]`, `[route.error_override]` and `[app.NAME.release.ID.error_override]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `statuses` | array of integers |  | The statuses whose responses are replaced. |
| `when` | string | `"all"` | When a listed response is replaced: `all` (the default), `empty_body`, `not_json` or `html_only`. |

(config-daemon-error-pages-status)=
## `[error_pages.STATUS]`

A custom error page: one entry of an `error_pages` table, keyed by status (`404`) or status and reason (`"503:maintenance"`). Exactly one of `file`, `template` and `path` is given.

The same keys apply at `[host.NAME.error_pages.STATUS]`, `[route.error_pages.STATUS]`, `[app.NAME.release.ID.error_pages.STATUS]` and `[overlay.NAME.routes.NAME.error_pages.STATUS]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `content_type` | string | `"text/html; charset=utf-8"` | The content type of a template. |
| `file` | string |  | A file sent as the body, with the content type its name implies. A release's page may be a path relative to its source directory; elsewhere the path is absolute. |
| `path` | string |  | An application path the edge asks for the page, as an internal sub-request that sees `REDIRECT_STATUS` and `muru.error_reason`. |
| `template` | string |  | A template the edge renders, with `{status}`, `{title}`, `{reason}`, `{description}`, `{request_id}`, `{retry_after}`, `{host}` and `{path}` filled in. |

(config-daemon-events)=
## `[events]`

The `[events]` table: the sinks events are pushed to.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `sink` | array of tables, {ref}`[[events.sink]] <config-daemon-events-sink>` |  | The sinks, each an `[[events.sink]]` entry. |

(config-daemon-events-sink)=
## `[[events.sink]]`

An `[[events.sink]]` entry: a webhook that receives events as signed CloudEvents requests.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `kind` | string | `"webhook"` | The kind of sink; only webhook for now. |
| `levels` | array of strings |  | Levels delivered (info, notice, warning, error); every level when empty. |
| `name` | string | required | The sink's name. |
| `secret` | string |  | The key deliveries are signed with. |
| `secret_file` | string |  | A file holding the key, instead of `secret`. |
| `types` | array of strings |  | Event types delivered, as patterns such as `release.*`; every type when empty. |
| `url` | string | required | Where deliveries go, an http:// or https:// URL. |

(config-daemon-host-name)=
## `[host.NAME]`

A virtual host, the `[host.NAME]` table.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `canonical` | string |  | The name the host is known by, for `use_canonical_name`. |
| `default` | boolean | `false` | Answer requests whose host matches no virtual host, which are otherwise refused with 421. |
| `error_override` | table, {ref}`[error_override] <config-daemon-error-override>` |  | Replace the application's own error responses with error pages. |
| `error_pages` | table of tables, {ref}`[error_pages.STATUS] <config-daemon-error-pages-status>` |  | Custom error pages, by status or status:reason. |
| `https_redirect` | boolean | `false` | Redirect plain HTTP requests to HTTPS. |
| `listeners` | array of strings |  | The listener addresses it answers on; every listener when empty. |
| `names` | array of strings |  | The names it answers to: exact names, or `*.example.com` for one label. Compared case-insensitively, without a port or trailing dot. |
| `rate_limits` | array of strings |  | The `[rate_limit.NAME]` tables applied to every request for the host, before a route's. |
| `tls` | table, {ref}`[host.NAME.tls] <config-daemon-host-name-tls>` |  | Its certificate and key, for TLS listeners. |
| `use_canonical_name` | boolean | `false` | Give the application the canonical name as SERVER_NAME rather than the name matched. |

(config-daemon-host-name-tls)=
## `[host.NAME.tls]`

A virtual host's certificate chain and private key, as PEM files.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `certificate` | string | required | The certificate chain, leaf first. A relative path is taken from the file's directory. |
| `key` | string | required | The private key. A relative path is taken from the file's directory. |

(config-daemon-limits)=
## `[limits]`

The `[limits]` table: sizes and durations as the configuration writes them.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `body_idle_timeout` | string | `"60s"` | Time a client may pause while sending a body. |
| `header_read_timeout` | string | `"30s"` | Time allowed for a complete request head. |
| `http2` | table, {ref}`[limits.http2] <config-daemon-limits-http2>` |  | HTTP/2 settings. |
| `keep_alive_timeout` | string | `"20s"` | Idle time before a keep-alive connection is closed. |
| `keep_alive_timeout_proxy` | string | `"75s"` | The same for connections from trusted proxies. |
| `max_body_size` | string | `"10MB"` | Largest request body, for example 10MB. |
| `max_connections` | integer | `65536` | Open connections per listener. |
| `max_connections_per_client` | integer | `256` | Open connections per client address; 0 disables it. |
| `max_header_bytes` | string | `"64KB"` | Largest request head, for example 64KB. |
| `max_headers` | integer | `100` | Most headers in a request. |
| `max_requests_per_connection` | integer | `0` | Requests served on one connection before it is closed; 0 for no limit. |
| `queue_timeout` | string | `"5s"` | Time a request may wait for a worker. |
| `request_deadline` | string | `"300s"` | Time a request may take to produce its response head. |
| `response_idle_timeout` | string | `"60s"` | Time a worker may pause mid-response. |
| `spool_quota` | string |  | What stored uploads may hold at once across every application; no limit without it. |

(config-daemon-limits-http2)=
## `[limits.http2]`

The `[limits.http2]` table.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `max_concurrent_streams` | integer | `128` | Concurrent streams per connection. |

(config-daemon-listener)=
## `[[listener]]`

A listener, one of the `[[listener]]` entries, in the daemon's own file.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `address` | string | required | The address, as IP:PORT. |
| `h2c` | array of strings | every client | On a listener without TLS, the clients that may speak HTTP/2 in cleartext (h2c), as addresses, ranges, loopback or private; every client when omitted, none when empty. The others are served HTTP/1.1. |
| `tls` | boolean | `false` | Whether TLS is terminated on it, with the certificates of the virtual hosts it serves, chosen by SNI. |

(config-daemon-logs)=
## `[logs]`

The `[logs]` table of the daemon's file: how the daemon rotates the log files it and its processes write, and whether its standard streams carry the time.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `compress` | string | `"zstd"` | How archives are compressed: `zstd` (the default), `gzip` or `off`. |
| `every` | string | `"daily"` | Rotate by the clock: `off`, `hourly`, `daily` (the default) or `weekly`, at the period's start in UTC. |
| `keep` | integer | `14` | Archives kept per log. |
| `rotate` | string | `"built-in"` | `built-in` (the default) or `external`, for logrotate. |
| `size` | string |  | Rotate a file that reaches this size, for example 100MB. |
| `timestamps` | boolean | `false` | Put the full UTC time in front of every line the daemon writes to standard output and standard error, as `--timestamps` does, for a supervisor that stamps nothing. Files always carry the time. |

(config-daemon-metrics)=
## `[metrics]`

The `[metrics]` table: where the daemon serves the Prometheus exposition.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `listen` | string |  | The address `/metrics` is served on, an internal one such as 127.0.0.1:9464; not served when omitted. |

(config-daemon-overlay-name)=
## `[overlay.NAME]`

An `[overlay.NAME]` table: changes to named routes and to applications, applied over the base configuration while the overlay is active, by its schedule or by hand.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `advisory` | boolean | `false` | Its schedule only raises overlay.would_activate events; it is activated by hand only. |
| `apps` | table of tables, {ref}`[overlay.NAME.apps.NAME] <config-daemon-overlay-name-apps-name>` |  | Changes to applications, by name. |
| `latch` | boolean | `false` | Once its schedule activates it, it stays active past the window until a person ends it. |
| `preconditions` | table, {ref}`[overlay.NAME.preconditions] <config-daemon-overlay-name-preconditions>` |  | What must hold for the overlay to activate. |
| `priority` | integer |  | Which of two active overlays setting the same field wins: the higher. Overlays that would conflict must both set one. |
| `routes` | table of tables, {ref}`[overlay.NAME.routes.NAME] <config-daemon-overlay-name-routes-name>` |  | Changes to routes, by the route's `name`. |
| `trigger` | table, {ref}`[overlay.NAME.trigger] <config-daemon-overlay-name-trigger>` |  | When the overlay is active on its own; by hand only without it. |
| `warm_up` | table, {ref}`[overlay.NAME.warm_up] <config-daemon-overlay-name-warm-up>` |  | What to start ahead of its schedule's windows. |

(config-daemon-overlay-name-apps-name)=
## `[overlay.NAME.apps.NAME]`

What an overlay changes on an application: its maintenance mode, and its scheduled capacity, for its default worker group or `group`. Capacity does not conflict between overlays: the highest floor and ceiling stand.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `answer` | string | `"read_only"` | How refused methods are answered, with the values a route's `methods.answer` takes. |
| `deny` | array of strings |  | Methods refused on each of the application's routes. |
| `freeze_deploys` | boolean | `false` | Refuse deploys and rollouts of the application unless overridden. |
| `group` | string | `"default"` | The worker group the capacity applies to; the default group without it. |
| `maintenance` | boolean |  | Maintenance mode while the overlay is active. |
| `max_workers` | integer |  | The most workers, a ceiling over the group's own. |
| `min_workers` | integer |  | The fewest workers, a floor over the group's own. |
| `mode` | string |  | The site mode on each of the application's routes. |
| `scale_in` | string | `"normal"` | `hold` keeps the group from scaling in; `normal` (the default) leaves it to the autoscaler's rules. |

(config-daemon-overlay-name-preconditions)=
## `[overlay.NAME.preconditions]`

What must hold for an overlay to activate.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `release_ready` | string |  | A release that must be ready, as app@release, or as release when the overlay changes routes of one application only. |

(config-daemon-overlay-name-routes-name)=
## `[overlay.NAME.routes.NAME]`

What an overlay changes on a route.

The same keys apply at `[overlay.NAME.routes.NAME.client_routes.NAME]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `answer` | string | `"read_only"` | How refused methods are answered, with the values a route's `methods.answer` takes. |
| `client_routes` | table of tables, {ref}`[overlay.NAME.routes.NAME] <config-daemon-overlay-name-routes-name>` |  | Routes for client classes at the route's host and path, by class: each a copy of the route with these changes, or the route for that class already there with them. |
| `deny` | array of strings |  | Methods refused, as the route's `methods.deny`. |
| `error_pages` | table of tables, {ref}`[error_pages.STATUS] <config-daemon-error-pages-status>` |  | Error pages, by status or status:reason, each in place of the route's page for the same key. |
| `mode` | string |  | The site mode. |
| `rate_limits` | array of strings |  | The rate limits, in place of the route's. |
| `target` | table, {ref}`[overlay.NAME.routes.NAME.target] <config-daemon-overlay-name-routes-name-target>` |  | What serves the route instead. |
| `traffic` | table, {ref}`[overlay.NAME.routes.NAME.traffic] <config-daemon-overlay-name-routes-name-traffic>` |  | The traffic policy: a pin sends every request to one release. |

(config-daemon-overlay-name-routes-name-target)=
## `[overlay.NAME.routes.NAME.target]`

What serves a route under an overlay: one of an application, a static directory or an upstream.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `app` | string |  | An application. |
| `static` | string |  | A directory the edge serves. |
| `upstream` | string |  | An HTTP upstream. |

(config-daemon-overlay-name-routes-name-traffic)=
## `[overlay.NAME.routes.NAME.traffic]`

An overlay's traffic policy for a route.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `pin` | string |  | Send every request to this release of the route's application. |
| `split` | table of integers |  | Shares by release, in place of the route's split. |

(config-daemon-overlay-name-trigger)=
## `[overlay.NAME.trigger]`

When an overlay is active on its own.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `external` | boolean | `false` | Whether the signed webhook of the `[overlay_webhook]` table may activate and end it. |
| `schedule` | table, {ref}`[overlay.NAME.trigger.schedule] <config-daemon-overlay-name-trigger-schedule>` |  | A window in time. |

(config-daemon-overlay-name-trigger-schedule)=
## `[overlay.NAME.trigger.schedule]`

A window in time: one-off, from `start` until `end` when it has one, or recurring, from each time `cron` names for `duration`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `cron` | string |  | When each window starts, as a cron expression such as '0 2 * * *' (see [`cron::Cron`]). |
| `duration` | string |  | How long each recurring window lasts, such as 1h. |
| `end` | string |  | When it ends; it stays active until ended or promoted without it. |
| `start` | string |  | When it starts, as a local date and time such as 2026-11-02T09:00, or with an offset. |
| `timezone` | string | the daemon's system time zone | The time zone local times are in, such as Australia/Sydney; the daemon's own zone without it. |

(config-daemon-overlay-name-warm-up)=
## `[overlay.NAME.warm_up]`

An overlay's warm-up: for `lead` before each window of its schedule, `start_release` runs, so a switch to it lands on warm workers.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `lead` | string | required | How long before the window, such as 30m. |
| `start_release` | string |  | The release to start, as app@release, or as release when the overlay changes routes of one application only. |

(config-daemon-overlay-webhook)=
## `[overlay_webhook]`

The `[overlay_webhook]` table: where the daemon takes signed requests from monitoring and alerting systems to activate and end the overlays that declare the external trigger.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `listen` | string | required | The address served, such as 0.0.0.0:9466. |
| `secret` | string |  | The key requests are signed with. |
| `secret_file` | string |  | A file holding the key, in place of `secret`. |

(config-daemon-proxy)=
## `[proxy]`

The `[proxy]` table: which peers are trusted to speak for the client, and through which headers.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `forwarded_headers` | string | `"keep"` | What becomes of the forwarding headers once applied. `keep`: The configured headers stay for the application, other forwarding headers are removed. `consume`: Every forwarding header is removed; the application sees only the resolved values. |
| `headers` | array of strings | `["forwarded", "x-forwarded-for", "x-forwarded-proto", "x-forwarded-host"]` | The header types applied from trusted peers: forwarded, x-forwarded-for, x-forwarded-proto, x-forwarded-host, x-forwarded-port, x-forwarded-prefix and x-real-ip. The first four when omitted. |
| `recursive` | boolean | `true` | Walk a chain of addresses from the right past trusted proxies to the first untrusted one, rather than take the rightmost. |
| `trusted` | array of strings |  | Addresses, CIDR ranges, or the words loopback, private and unix. Nothing is trusted when empty. |

(config-daemon-rate-limit-name)=
## `[rate_limit.NAME]`

A `[rate_limit.NAME]` table: a token bucket for each key, which requests of the matching classes draw from.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `action` | string | `"reject"` | `reject` (the default) answers 429, `delay` holds the request up to `max_delay` first, `log` only counts. |
| `burst` | integer | the rate's count | How many requests a full bucket allows at once. |
| `headers` | boolean | `false` | Send the RateLimit-Policy and RateLimit fields with a 429. |
| `ipv4_prefix` | integer | `32` | The leading bits of an IPv4 address that make the key (32). |
| `ipv6_prefix` | integer | `64` | The leading bits of an IPv6 address that make the key (64). |
| `key` | string or array of strings | `"address"` | What a bucket is kept for: `address` (the default), `client_class`, `header:NAME`, `cookie:NAME`, or a list of them. |
| `match` | table, {ref}`[rate_limit.NAME.match] <config-daemon-rate-limit-name-match>` |  | Which requests draw from the buckets; all of them without it. |
| `max_delay` | string | `"1s"` | The longest a request is held under `delay`. |
| `rate` | string | required | How fast a bucket fills, for example 2/s, 100/m or 1000/h. |

(config-daemon-rate-limit-name-match)=
## `[rate_limit.NAME.match]`

The requests a rate limit or route applies to.

The same keys apply at `[route.match]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `client_class` | array of strings |  | Client classes. |

(config-daemon-route)=
## `[[route]]`

A route, one of the `[[route]]` entries: a path prefix of a virtual host to an application.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `accept_redirects_from` | array of strings |  | Applications other than the route's own (or a static route's fallback) whose internal redirects may reach the route. A static route with no fallback needs it for any redirect to reach it. |
| `app` | string |  | The application whose serving release takes the requests; empty for a static or upstream route. |
| `auth` | table, {ref}`[route.auth] <config-daemon-route-auth>` |  | An authorisation decision the edge asks an application for before it serves the route. |
| `cache` | table, {ref}`[route.cache] <config-daemon-route-cache>` |  | The cache headers files are sent with. |
| `case` | string | `"strict"` | `strict` (the default), which matches names as they are on disk, or `insensitive`. |
| `error_override` | table, {ref}`[error_override] <config-daemon-error-override>` |  | Replace the application's own error responses with error pages. |
| `error_pages` | table of tables, {ref}`[error_pages.STATUS] <config-daemon-error-pages-status>` |  | Custom error pages, by status or status:reason. |
| `fallback` | table, {ref}`[route.fallback] <config-daemon-route-fallback>` |  | What takes a request no file answers. |
| `hash` | array of strings | `["sha256"]` | The digests computed while receiving: sha256 (the default), sha384, sha512 or sha1; an empty list for none. |
| `host` | string | required | The virtual host. |
| `index` | array of strings | `["index.html"]` | The files tried, in order, when a directory is requested. |
| `internal` | boolean | `false` | Reachable only through internal redirects: a client's request for the path is answered 404. |
| `match` | table, {ref}`[rate_limit.NAME.match] <config-daemon-rate-limit-name-match>` |  | The client classes the route takes; every request without it. Among routes with the same path, one naming the request's class comes first. |
| `max_body_size` | string | `max_body_size` in `[limits]` | The largest request body, for example 1GB, replacing the daemon's `[limits]` value for the route. On an upload route it bounds the body as received, files included. |
| `max_file_size` | string |  | The largest uploaded file, for example 512MB; no limit without it. |
| `max_files` | integer | `100` | The most files in one request. |
| `methods` | table, {ref}`[route.methods] <config-daemon-route-methods>` |  | Methods the route refuses. |
| `mode` | string |  | The site mode passed to the application as muru.mode, for example read-only. |
| `name` | string |  | A name for the route, unique among routes, by which overlays change it. |
| `path` | string | `"/"` | The path prefix, `/` for everything. The longest prefix wins. |
| `precheck` | boolean | `false` | Ask the application with the request's headers alone first, and read the body only when it answers 2xx. |
| `rate_limits` | array of strings |  | The `[rate_limit.NAME]` tables applied to the route's requests, after the host's. |
| `redirect_slash` | boolean | `true` | Whether a directory asked for without a trailing slash is redirected to the slash form. |
| `spool_quota` | string | `"10GB"` | What the application's stored uploads may hold at once when a request on the route arrives. |
| `static` | string |  | The directory served by the edge, instead of an application. |
| `traffic` | table, {ref}`[route.traffic] <config-daemon-route-traffic>` |  | How requests are shared between the application's releases; without it every request goes to the serving release. |
| `upload` | string |  | `multipart` or `raw` makes the edge receive and store uploads before the application sees the request; off without it. |
| `upstream` | string |  | The HTTP upstream that takes the requests, instead of an application. |

(config-daemon-route-auth)=
## `[route.auth]`

A route's authentication sub-request: the edge sends a bodiless GET to an application path and serves the route only when that answers 2xx.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `app` | string |  | The application that decides; empty when an upstream does. |
| `cache` | table, {ref}`[route.auth.cache] <config-daemon-route-auth-cache>` |  | Remember allowing decisions for a while (off without it). |
| `copy_headers` | array of strings |  | Response headers of an allowing answer added to the request as trusted headers; a client's own copies are always removed. |
| `path` | string | required | The path asked, usually of an internal mount or route. |
| `timeout` | string | `"2s"` | How long the handler has, queueing included, for example 2s. |
| `upstream` | string |  | The HTTP upstream that decides, instead of an application. |

(config-daemon-route-auth-cache)=
## `[route.auth.cache]`

How long and by what an allowing decision is remembered.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `key` | array of strings | required | What a decision is keyed by: `cookie:NAME` and `header:NAME` entries. A request with none of them present is never served from the cache. |
| `ttl` | string | required | How long a decision stands, for example 30s. |

(config-daemon-route-cache)=
## `[route.cache]`

A static route's cache headers.

The same keys apply at `[app.NAME.release.ID.static.cache]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `except` | array of strings |  | Files that get no-cache instead, by pattern on their path within the route. |
| `immutable` | boolean | `false` | Whether files are marked immutable. |
| `max_age` | string |  | How long clients may keep a file, for example 1y or 1h. |

(config-daemon-route-fallback)=
## `[route.fallback]`

A static route's fallback: an application, or a file within the directory.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `app` | string |  | The application that takes requests no file answers. |
| `file` | string |  | The file served for paths with no file of their own. |
| `upstream` | string |  | The HTTP upstream that takes them instead. |

(config-daemon-route-methods)=
## `[route.methods]`

A route's method restrictions.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `answer` | string | `"read_only"` | `read_only` (the default) answers 503 read_only; `method_not_allowed` answers 405 with the methods left in Allow. |
| `deny` | array of strings |  | The methods refused, for example POST, PUT, PATCH and DELETE. |

(config-daemon-route-traffic)=
## `[route.traffic]`

A route's traffic policy: the share of requests each release takes, the key requests are bucketed by, and stickiness.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `classes` | table of strings |  | The release, by id, or `upstream:NAME`, that takes every request of a client class, ahead of stickiness and the split. |
| `hash_key` | string | `"cookie"` | What a request is bucketed by: `cookie` (the default, an anonymous id cookie the edge issues), `header:NAME`, `cookie:NAME`, or `client` (the client address). A missing header or cookie falls back to the client address. |
| `salt` | string | the route's host and path | Mixed into the hash, so two routes split the same users differently. |
| `split` | table of integers |  | Percent of requests to each release, by id; the application's serving release takes the rest. A release at 0 is reached only by preview or by a sticky cookie. |
| `sticky` | boolean | `false` | Whether a client keeps the release it first got, through a cookie. |
| `sticky_grace` | string | `"10m"` | How long sticky clients keep a release that is leaving the route alive, for example 10m. |

(config-daemon-secrets)=
## `[secrets]`

The `[secrets]` table of the daemon's file.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `store` | table of tables, {ref}`[secrets.store.NAME] <config-daemon-secrets-store-name>` |  | Stores by name. |

(config-daemon-secrets-store-name)=
## `[secrets.store.NAME]`

One `[secrets.store.NAME]` table.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `command` | array of strings |  | For an exec store, the command and arguments, with `{key}` where the key goes. |
| `kind` | string | `""` | `file`, `env` or `exec`. |
| `path` | string | `secrets/NAME` in the state directory | For a file store, the directory; the daemon's secrets directory under the store's name otherwise. |
| `timeout` | string | `"5s"` | For an exec store, how long the command may take. |

(config-daemon-shutdown)=
## `[shutdown]`

The `[shutdown]` table of the daemon's file: what SIGTERM and SIGINT mean to the daemon, and how a shutdown runs.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `budget` | string | `"8s"` | How long the whole shutdown may take, the delay, the drain, the workers and the edge together; what has not finished by then is killed. |
| `delay` | string | `"0s"` | How long the edge keeps accepting after the signal, closing each connection after its response so clients reconnect elsewhere, for example 5s. |
| `on_sigterm` | string | `shutdown` as a container's main process, else `detach` | `shutdown`, which drains and stops every release and the edge before the daemon exits, or `detach`, which leaves them serving for the next daemon; shutdown when the daemon is a container's main process, detach otherwise. |

(config-daemon-tools)=
## `[tools]`

The `[tools]` table of the daemon's file: uv, and how interpreters are installed.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `offline` | boolean | `false` | No network at all: nothing is downloaded, and wheels come from `wheel_dir`. |
| `python_downloads` | string | `"automatic"` | Whether a Python the node lacks is downloaded for a release that requests it. `automatic`: Download a Python a release requests and the node lacks. `never`: Never download; a request the node cannot satisfy fails the release. |
| `python_install_dir` | string | `python` in the state directory | Where managed interpreters go; a shared read-only directory for several daemons. |
| `uv` | string |  | The uv to run, in place of the lookup order (`MURU_UV`, a uv beside muructl, uv on PATH, the managed copy). |
| `uv_config` | string |  | A `uv.toml` passed to builds as their configuration file, for mirrors and TLS settings; the project's own `[tool.uv]` still applies. |
| `uv_version` | string |  | The uv versions accepted, as a specifier such as `>=0.9`; the daemon reports what an older one cannot do. |
| `wheel_dir` | string |  | A local directory of wheels for the runtime library and for builds when offline. |

(config-daemon-upstream-name)=
## `[upstream.NAME]`

An HTTP upstream, one of the `[upstream.NAME]` tables: an unmanaged HTTP server that routes, fallbacks, traffic shares and authentication sub-requests can target.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `addresses` | array of strings | required | Where the server listens: `host:port` entries, or `unix:/path` for a Unix socket. Requests take the addresses in turn, and one that fails is set aside for a while. |
| `connect_timeout` | string | `"5s"` | How long a connection may take to open, for example 5s. |
| `health` | table, {ref}`[upstream.NAME.health] <config-daemon-upstream-name-health>` |  | Active health checks and passive ejection. |
| `protocol` | string | `"http1"` | `http1` (the default), `h2c` (HTTP/2 in the clear, by prior knowledge) or `h2` (HTTP/2 over TLS). |
| `sendfile_header` | string |  | A second response header honoured as `X-Muru-Sendfile` is, such as `X-Sendfile`. |
| `sendfile_roots` | array of strings |  | Directories a path in a trusted `X-Muru-Sendfile` response header must lie in for the edge to serve the file; with none every hint is refused. |
| `tls` | table, {ref}`[upstream.NAME.tls] <config-daemon-upstream-name-tls>` |  | Connect with TLS; `h2` implies it. |
| `trust_muru_headers` | boolean | `false` | Honour X-Muru-Redirect from the upstream, as from an application: its X-Muru-* response headers are otherwise removed. |

(config-daemon-upstream-name-health)=
## `[upstream.NAME.health]`

An upstream's health checks: probes of each address, and the ejection of one whose requests fail.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `eject_after` | integer | `3` | Failed requests in a row before an address is ejected; 0 never ejects. |
| `eject_for` | string | `"30s"` | How long an ejected address is left out before a request tries it again, for example 30s; doubled each time the trial fails, up to ten times as long. |
| `healthy_after` | integer | `2` | Successful probes in a row before it is healthy again. |
| `interval` | string | `"10s"` | How often each address is probed, for example 10s. |
| `path` | string |  | The path probed with a GET, healthy on 2xx or 3xx; without it a connection that opens is healthy. |
| `timeout` | string | `"2s"` | How long a probe may take, for example 2s. |
| `unhealthy_after` | integer | `3` | Failed probes in a row before an address is unhealthy. |

(config-daemon-upstream-name-tls)=
## `[upstream.NAME.tls]`

How an upstream's TLS connections are made.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `ca` | string |  | A PEM file of the certificates trusted, by absolute path; the public roots without it. |
| `server_name` | string | the address's host | The name the certificate must carry, sent as SNI; the address's host without it. |
| `verify` | boolean | `true` | Check the certificate. |
