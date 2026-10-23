# Appendix C. Configuration reference

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

This is the initial key list. The generated reference (section 18.3) becomes authoritative once muru-config exists. Sizes accept units (KB, MB, GB, KiB, MiB, GiB) and durations accept ms, s, m, h.

## C.1 File structure

| Table | Where | Holds |
| --- | --- | --- |
| `[daemon]` | daemon file | paths (state, run, logs), admin socket and TCP access, roles, budgets, offline mode |
| `[tools]` | daemon file | uv path and version constraints |
| `[[edge.pool]]`, `[[listener]]` | daemon file | listeners, protocols, PROXY protocol, processes, threads |
| `[proxy]` | daemon, host or listener | trusted proxies and header types |
| `[limits]` | global, host, route, mount, group | request limits and timeouts |
| `[logging.access]`, `[logging.apps]`, `[logging.python]` | global, application, release | sinks, formats, rotation, Python defaults |
| `[[events.sink]]` | daemon file | event sinks |
| `[host.NAME]` | application files | virtual hosts |
| `[[route]]` | application files | routes |
| `[upstream.NAME]` | application files | HTTP upstreams |
| `[app.NAME]` | application files | application defaults, reload, schedules |
| `[app.NAME.release.ID]` | application files | release definition |
| `[app.NAME.release.ID.mounts.NAME]` | application files | mounts |
| `[app.NAME.release.ID.groups.NAME]` | application files | worker groups |
| `[tool.muru]` | the application's pyproject.toml or muru.toml | application-shipped defaults |
| `[overlay.NAME]` | application files | overlays and triggers |
| `[[client_class]]` | daemon or application files | classification rules |
| `[rate_limit.NAME]` | daemon or application files | rate limits |
| `[app.NAME.release.ID.variants.NAME]` and .groups.NAME under it | application files | release variants and their worker groups |

## C.2 Key settings

| Key | Scope | Default | Section |
| --- | --- | --- | --- |
| entrypoint, interface, factory, factory_phase | release, mount | interface detected, factory_phase worker | 6.8 |
| source or image, python, build, runtime_source | release | none, none, none, image | 8.1 to 8.5 |
| preload | release | auto (preload with the import check, falling back to per-worker imports) | 6.1, 6.2 |
| processes, threads, sync_threads | release, group, mount | 2, 8 for WSGI, 1 loop for ASGI | 6.4 |
| hooks | release | none | 6.3 |
| paths, mounts, optional | group | none, all mounts, false | 7.1 |
| path, internal, cgi_local_redirect, accel_redirect, concurrency, max_connections | mount | none, false, false, false, 1000, 1000 | 6.7, 4.10 |
| scaling.mode, min_workers, max_workers, min_spare, max_spare | group | static | 7.2, 7.4 |
| scaling windows and budgets (scale_in_window, hold_after_growth, reap_interval, idle_reap_after, min_lifetime, max_step_up, churn_budget) | group | 10m, 5m, 60s, 10m, max(15m, 30 x ready time), 4, 6 per hour | 7.4 |
| max_requests, max_lifetime, max_surge | group | off, off, 1 | 7.7 |
| memory.measure, soft_limit, soft_for, reclaim_first, hard_limit, check_interval, malloc_arena_max, allocator | group | uss, off, 2m, true, off, 5s, unset, system | 7.7 |
| timeouts (queue_timeout, request_deadline, soft_timeout, hard_timeout, hard_drain_grace, shutdown_timeout, loop_stall_timeout, idle_timeout) | group, route | 5s, 300s, off, off, 10s, 5s, 10s, 60s | 7.6 |
| response.flush, coalesce_min_bytes, coalesce_max_delay, first_block | release, route | coalesce, 16KiB, 10ms, immediate | 5.4 |
| max_body_size, buffer_body, chunked_body, buffer_response | route and wider | 10MB, off, buffer, on | 4.6 |
| max_header_bytes, max_headers, header_read_timeout | listener, host | 64KiB, 100, 30s | 4.6 |
| upload, max_file_size, max_files, hash, precheck, spool quota | route | off, unlimited, 100, sha256, false, 10GB per application | 4.11 |
| static (directory or release_path), index, fallback, listing, case (strict by default), cache, follow_symlinks, stat_cache, sendfile_roots | route, mount | none | 4.9 |
| error_pages, error_override | global, host, application, route | built-in, off | 4.12 |
| accept_redirects_from | route, mount | none | 4.10 |
| names, listeners, tls, canonical, https_redirect, use_canonical_name | host | none, all, none, none, false, false | 4.3 |
| traffic: split, sticky, hash_key, salt, sticky_grace | route | 100% to newest ready release, off, cookie, per route, 10m | 9.4 |
| retention of superseded releases | application | 30m | 9.1 |
| reload.mode, paths, include, exclude, respect_gitignore, debounce, poll | application | off, source, `**/*.py`, none, true, 300ms, false | 9.8 |
| schedules (cron or start and end, duration, timezone, min_workers, max_workers, warm_up, scale_in, freeze_deploys) | application, group | none | 7.5 |
| wsgi_multiprocess, wsgi_multithread | release | auto | 6.5 |
| logging.python level, format, request_id, capture_warnings, loggers | global, application, release | WARNING (Python default), standard, false, false, none | 11.4 |
| rotate (size, every, keep, compress, naming), reopen_on_rename, on_overflow | log sinks | daily, 14, zstd, built-in | 11.3 |
| readiness, liveness (path, timeout, expect, required, interval, failures), checks | mount, release | none (built-in signals only) | 9.11 |
| compression (enabled, encodings, min_size, types, level), decompress_requests | global, host, route | enabled, br zstd gzip, 1KB, text and JSON types, default, false | 4.17 |
| max_connections, max_connections_per_client, high_water, min_idle_timeout, overload_reserve, keep_alive_timeout, keep_alive_timeout_proxy, max_requests_per_connection, `http2.*`, `websocket.*` | listener, mount | see 4.18 | 4.18 |
| auth (app, path, timeout, copy_headers, cache) | route | none | 4.16 |
| response_headers (add, remove), request_headers (add, remove) | host, route | none | 4.3 |
| user, group, umask, limits | application | daemon's user, none | 10.9 |
| `[secrets.store.NAME]` (kind, command, timeout), env values with secret = { store, key } and optional as = 'file' | daemon, release | file store in the secrets directory | 10.8 |
| bytecode | release, variant | redirect | 8.2 |

## C.3 Error statuses and reasons

| Status | Reasons |
| --- | --- |
| 400, 414, 431 | malformed, uri_too_long, headers_too_large |
| 404 | no_route, internal_only |
| 408, 411, 413, 421, 426 | client_timeout, length_required, body_too_large, misdirected, upgrade_not_allowed |
| 429 | rate_limited |
| 500 | app_error, redirect_loop |
| 502 | worker_crashed, upstream_failed |
| 503 | queue_full, no_workers, cold_start_timeout, release_failed, draining, maintenance, read_only, auth_unavailable, edge_overloaded |
| 504 | deadline, soft_timeout, hard_timeout |
| 507 | upload_spool_full |
| 403, 405 | blocked (client class or rule), method_not_allowed |

## C.4 Event types

| Category | Types (prefix dev.muru.) |
| --- | --- |
| Configuration | config.applied, route.changed, traffic_policy.changed, maintenance.changed |
| Releases | release.created, release.state_changed, rollout.started, rollout.step, rollout.gate_failed, rollout.completed, rollout.rolled_back |
| Processes | host.started, host.exited, worker.started, worker.ready, worker.exited, worker.recycled, worker.timeout_killed |
| Capacity | scaling.decision, scaling.denied_by_budget, churn_budget.exhausted, schedule.activated, schedule.expired |
| Platform | daemon.started, daemon.upgraded, edge.replaced, snapshot.applied, snapshot.rejected, certificate.renewed, certificate.expiring, logs.reopened |
| Warnings | preload.unsafe_import, preload.fell_back, release.import_failed, memory.leak_suspected, upload_spool.full, upstream.unhealthy |
| Overlays | overlay.activated, overlay.ended, overlay.skipped, overlay.would_activate, overlay.promoted |
| Clients | ratelimit.enforcing, client_class.list_reloaded |
