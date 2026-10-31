# Appendix C. Configuration reference

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

This is the initial key list. The generated reference (section 18.3) becomes authoritative once muru-config exists. Sizes accept units (KB, MB, GB, or KiB, MiB, GiB, all powers of 1024 as mod_wsgi counts them, so 10MB is 10485760 bytes) and durations accept ms, s, m, h.

## C.1 File structure

| Table | Where | Holds |
| --- | --- | --- |
| `[daemon]` | daemon file | paths (state, run, logs), admin socket and TCP access, roles, budgets, offline mode, debug_exec (section 11.7) |
| `[tools]` | daemon file | uv path and version constraints |
| `[[edge.pool]]`, `[[listener]]` | daemon file | listeners, protocols, processes, threads |
| `[proxy]` | daemon, host or listener | trusted proxies and header types |
| `[metrics]` | daemon file | the internal address `/metrics` is served on (`listen`), none by default |
| `[dashboard]` | daemon file | the loopback address the dashboard and JSON gateway are served on (`listen`), none by default, and whether signed-in sessions may act (`actions`, false) |
| `[overlay_webhook]` | daemon file | the address of the overlays' signed webhook (`listen`) and its key (`secret` or `secret_file`), none by default |
| `[limits]` | global, host, route, mount, group | request limits and timeouts |
| `[logging.access]`, `[logging.apps]`, `[logging.python]` | global, application, release | sinks, formats, rotation, Python defaults |
| `[shutdown]` | daemon file | what SIGTERM means (`on_sigterm`), the delay the edge keeps accepting for and the budget the whole shutdown fits (section 13.3) |
| `[[events.sink]]` | daemon file | event sinks |
| `[host.NAME]` | application files | virtual hosts |
| `[[route]]` | application files | routes |
| `[upstream.NAME]` | application files | HTTP upstreams |
| `[app.NAME]` | application files | application defaults, reload, schedules |
| `[app.NAME.release.ID]` | application files | release definition |
| `[app.NAME.release.ID.mounts.NAME]` | application files | mounts |
| `[[app.NAME.release.ID.static]]` | application files, or `[[tool.muru.static]]` | the release's own directories of files (section 4.9) |
| `[app.NAME.release.ID.groups.NAME]` | application files | worker groups |
| `[tool.muru]` | the application's pyproject.toml or muru.toml | application-shipped defaults |
| `[tool.muru.run]`, `[run]` | the application's pyproject.toml or muru.toml, and muru.local.toml | how `muructl run` serves the project (section 13.1): a key for each flag of the command that describes the serving, ignored by the daemon |
| `[overlay.NAME]` | application files | overlays and triggers |
| `[[client_class]]` | daemon or application files | classification rules |
| `[rate_limit.NAME]` | daemon or application files | rate limits |
| `[app.NAME.release.ID.variants.NAME]` and .groups.NAME under it | application files | release variants and their worker groups |

## C.2 Key settings

| Key | Scope | Default | Section |
| --- | --- | --- | --- |
| entrypoint, interface (auto, wsgi, asgi or grpc), factory, factory_phase | release, mount | interface detected, factory_phase worker | 6.8, 6.13 |
| reflection | release | false (gRPC server reflection for the registered services) | 6.13 |
| source or image, python, build, runtime_source | release | none, none, none, image | 8.1 to 8.5 |
| preload | release | auto (preload with the import check, falling back to per-worker imports) | 6.1, 6.2 |
| processes, threads, sync_threads | release, group, mount | 2, 8 for WSGI, 1 loop for ASGI | 6.4 |
| threads on a mount: a number for a pool of its own, or `{ reserved, max }` to borrow the shared pool's threads beyond the reserved ones | mount (WSGI, and a number on gRPC) | the worker's shared pool | 6.7, 6.13 |
| hooks | release | none | 6.3 |
| kind, profile, command, cwd, bind, instances, readiness (path, timeout), stop_signal | release | workers, none, none, source, unix, 1, none and 60s, the profile's | 9.9 |
| paths, mounts, optional | group | none, all mounts, false | 7.1 |
| match.client_class | group, route | none (every class) | 4.14 |
| match.host, match.method | group | none (every host, every method) | 7.1 |
| path, internal, cgi_local_redirect, accel_redirect, concurrency, max_connections | mount (cgi_local_redirect and accel_redirect also on the release, for the default mount and as the mounts' default) | none (required except for a gRPC mount, which takes none), false, false, false, 1000, 1000 | 6.7, 4.10, 6.13 |
| path, dir, index, fallback, cache, redirect_slash, case | static entry (`[[tool.muru.static]]` or `[[app.NAME.release.ID.static]]`) | none (path and dir required), index.html, none, none, true, strict | 4.9 |
| scaling.mode, min_workers, max_workers, min_spare, max_spare, advisory | group | static, 1, required by spare and target, 1, max(3, 25% of max_workers), false | 7.2, 7.4 |
| scaling targets (slot_utilization, queue_wait_p95) | group, target mode | 0.7, off | 7.2, 7.3 |
| scaling windows and budgets (scale_out_window, scale_in_window, hold_after_growth, reap_interval, idle_reap_after, min_lifetime, max_step_up, churn_budget) | group | 5s, 10m, 5m, 60s, 10m, max(15m, 30 x ready time), 4, 6 per hour | 7.4 |
| h2c (the clients that may speak HTTP/2 in cleartext) | listener | every client | 6.13 |
| grpc_web_origins (CORS for gRPC-Web) | release | none | 6.13 |
| budget.workers, budget.memory, budget.cpu | `[daemon]` | off | 7.4 |
| on_sigterm, delay, budget | `[shutdown]` | shutdown when the daemon is a container's main process and detach otherwise, 0s, 8s | 13.3 |
| min_workers = 0 (scale to zero), host_idle_timeout | group; host_idle_timeout in the release's own scaling table | none, off | 7.4 |
| priority (the order in which the node's budget admits scale-ups, highest first) | application | 0 | 7.4 |
| max_requests, max_lifetime, max_surge | group | off, off, 1 | 7.7 |
| memory.measure, soft_limit, soft_for, reclaim_first, hard_limit, check_interval, malloc_arena_max, allocator | group | uss, off, 2m, true, off, 5s, unset, system | 7.7 |
| timeouts (queue_timeout, request_deadline, soft_timeout, hard_timeout, hard_drain_grace, shutdown_timeout, loop_stall_timeout, gil_stall_timeout, idle_timeout) | group, route | 5s, 300s, off, off, 10s, 5s, 10s, 30s, 60s | 7.6 |
| response.flush, coalesce_min_bytes, coalesce_max_delay, first_block | release, route | coalesce, 16KiB, 10ms, immediate | 5.4 |
| max_body_size, buffer_body, chunked_body, buffer_response | route and wider | 10MB, off, buffer, on | 4.6 |
| max_header_bytes, max_headers, header_read_timeout | listener, host | 64KiB, 100, 30s | 4.6 |
| upload, max_file_size, max_files, hash, precheck, spool_quota | route (spool_quota also in the daemon's [limits], for every application together) | off, unlimited, 100 (at most 500), [sha256], false, 10GB per application (none for the node) | 4.11 |
| static (directory or release_path), index, fallback, listing, case (strict by default), cache, follow_symlinks, stat_cache | route | none | 4.9 |
| sendfile_roots, sendfile_header | release, mount, upstream | none, none | 4.9 |
| rate_limits | host, route | none | 4.15 |
| name | route | none | 9.10 |
| methods (deny, answer), mode | route | none, read_only, none | 4.15 |
| error_pages, error_override | global, host, application (release), route | built-in, off | 4.12 |
| maintenance (exempt_paths, exempt_addresses, bypass_cookie, retry_after) | release | none, none, none, 60s | 4.12 |
| accept_redirects_from, internal | route, mount | none, false | 4.10 |
| names, listeners, tls, canonical, https_redirect, use_canonical_name | host | none, all, none, none, false, false | 4.3 |
| traffic: split, sticky, hash_key, salt, sticky_grace, classes | route | 100% to newest ready release, off, cookie, per route, 10m, none | 9.4, 4.14 |
| retention of superseded releases | application | 30m | 9.1 |
| reload.mode, paths, include, exclude, respect_gitignore, debounce, poll | application | off, source, `**/*.py`, none, true, 300ms, false | 9.8 |
| schedules (name, start and end or cron and duration, timezone, group, min_workers, max_workers, warm_up, scale_in, freeze_deploys) | application, group | none | 7.5 |
| wsgi_multiprocess, wsgi_multithread | release | auto | 6.5 |
| logging.python level, format, request_id, capture_warnings, loggers | global, application, release | WARNING (Python default), standard, false, false, none | 11.4 |
| rotate (size, every, keep, compress, naming), reopen_on_rename, on_overflow | log sinks | daily, 14, zstd, built-in | 11.3 |
| readiness, liveness (path, timeout, expect, required, interval, failures), checks | mount, release | none (built-in signals only) | 9.11 |
| compression (enabled, encodings, min_size, types, level), decompress_requests | global, host, route | enabled, br zstd gzip, 1KB, text and JSON types, default, false | 4.17 |
| max_connections, max_connections_per_client, high_water, min_idle_timeout, overload_reserve, keep_alive_timeout, keep_alive_timeout_proxy, max_requests_per_connection, `http2.*`, `websocket.*` | listener, mount | see 4.18 | 4.18 |
| auth (app or upstream, path, timeout, copy_headers, cache) | route | none | 4.16 |
| upstream (route target), fallback.upstream, traffic.split upstream:NAME | route | none | 4.13 |
| addresses, protocol, tls (ca, server_name, verify), health (path, interval, timeout, unhealthy_after, healthy_after, eject_after, eject_for), trust_muru_headers, connect_timeout | upstream | required, http1, none, none (ejection after 3 failures for 30s), false, 5s | 4.13 |
| response_headers (add, remove), request_headers (add, remove) | host, route | none | 4.3 |
| user, group, umask, limits | application | daemon's user, none | 10.9 |
| `[secrets.store.NAME]` (kind, command, timeout), env values with secret = { store, key } and optional as = 'file' | daemon, release | file store in the secrets directory | 10.8 |
| bytecode | release, variant | redirect | 8.2 |

## C.3 Error statuses and reasons

| Status | Reasons |
| --- | --- |
| 400, 414, 431 | malformed, upload_malformed, uri_too_long, headers_too_large |
| 404 | no_route, internal_only, sendfile_missing |
| 408, 411, 413, 421, 426 | client_timeout, length_required, body_too_large, upload_too_large, too_many_files, misdirected, upgrade_not_allowed |
| 429 | rate_limited |
| 500 | app_error, redirect_loop, sendfile_refused |
| 502 | worker_crashed, worker_stalled, worker_unavailable, upstream_failed |
| 503 | queue_full, no_workers, cold_start_timeout, release_failed, draining, maintenance, read_only, auth_unavailable, edge_overloaded, mount_not_loaded |
| 504 | deadline, soft_timeout, hard_timeout |
| 507 | upload_spool_full |
| 403, 405 | blocked (client class or rule), method_not_allowed |
| any | error_override (an application response the override replaced, with its own status) |
| 401, 403, 3xx | auth_denied (an authentication handler's own refusal or redirect, with its status) |

## C.4 Event types

| Category | Types (prefix dev.muru.) |
| --- | --- |
| Configuration | config.applied, route.changed, traffic_policy.changed, maintenance.changed |
| Releases | release.created, release.state_changed, rollout.started, rollout.step, rollout.gate_failed, rollout.completed, rollout.rolled_back, deploy.freeze_overridden, release.idle, release.woken |
| Processes | host.started, host.exited, worker.started, worker.ready, worker.exited, worker.recycled, worker.timeout_killed, worker.loop_stalled, worker.gil_stalled, worker.restarted, server.started, server.exited, server.failed |
| Capacity | scaling.decision, scaling.denied_by_budget, churn_budget.exhausted, schedule.activated, schedule.expired, schedule.over_budget |
| Platform | daemon.started, daemon.upgraded, edge.replaced, snapshot.applied, snapshot.rejected, certificate.renewed, certificate.expiring, logs.reopened, clock.unsynchronised, clock.synchronised |
| Warnings | preload.unsafe_import, preload.fell_back, release.import_failed, memory.leak_suspected, upload_spool.full, upstream.unhealthy, upstream.unusable |
| Overlays | overlay.activated, overlay.ended, overlay.skipped, overlay.would_activate, overlay.warming, overlay.promoted |
| Clients | ratelimit.enforcing, client_class.list_reloaded |
| Dashboard | dashboard.login_issued, dashboard.action |
| Debugging | request.soft_timeout, debug.profile, debug.trace, debug.memory, debug.exec, debug.breakpoint, debug.breakpoint_expired, debug.attached, debug.detached |
