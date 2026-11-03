<!-- Generated from crates/muru-config by tests/reference.rs; do not edit. -->

# The muructl serve configuration file

The file `muructl serve --config` or `MURU_SERVE_CONFIG` names: the keys of the project's `[tool.muru.serve]` table at its top level, taking precedence over that table. A command line flag or a `MURU_` variable takes precedence over the file.

The JSON Schema of the same keys, for an editor that checks TOML against one, is `https://docs.muruhq.dev/en/latest/schema/serve.schema.json`.

(config-serve)=
## Top level

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
| `trusted_proxy` | array of strings | loopback peers, for their forwarding headers alone | The proxies whose forwarding headers are applied: addresses, CIDR ranges, or loopback, private and unix. A proxy named here also gets a proxy's share of connections and keep-alive timeout. |
