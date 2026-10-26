# Appendix A. muructl command reference

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

Commands that act on a running system use the admin API (Appendix B). Common flags: --socket (or MURU_SOCKET) or --server to choose the daemon, --json for machine-readable output, --yes to skip confirmation. Releases are addressed as app@release; muructl release wait takes --state and --timeout.

| Group | Command | What it does | Section |
| --- | --- | --- | --- |
| Development | muructl run [ENTRYPOINT or PATH] | Run with an embedded controller and edge. Flags: --interface, --factory, --python, --processes, --threads, --sync-threads, --concurrency, --preload, --hooks, --timeout, --max-body-size, --dashboard, --bind, --port, --allow-public, --https, --https-only, --https-port, --tls-cert, --tls-key, --host, --access-log, --access-log-exclude, --slow, --single, --reload, --reload-dir, --reload-include, --reload-exclude, --print-config | 13.1 |
| Development | muructl check [PATH or RELEASE] | Load in a throwaway host and validate entrypoints, hooks, mounts and environment | 6.8 |
| Development | muructl exec RELEASE -- COMMAND | One-off command with the release's interpreter, environment, user and directory | 6.8 |
| Daemon | muructl daemon run | Run the daemon in the foreground. Flags: --listen, and the location flags --config, --state-dir, --run-dir, --log-dir, --cache-dir and --system (section 13.8) | 13.3 |
| Daemon | muructl daemon start, stop, restart, status, reload | Background daemon without a service manager. start takes the flags of daemon run; stop takes --shutdown and --timeout; reload re-reads the configuration file | 13.3 |
| Daemon | muructl daemon reload | Re-read the daemon configuration file (also SIGHUP) | 10.3 |
| Daemon | muructl daemon upgrade | Re-exec from the installed binary and replace edges | 13.4 |
| Daemon | muructl daemon backup, restore | Save or load the state store | 13.5 |
| Daemon | muructl up | Ensure a daemon is running for the current user or project | 13.2 |
| Daemon | muructl edge run | Internal, started by the daemon | 2.3 |
| Configuration | muructl apply -f FILE or DIR | Apply declarative configuration. Flags: --dry-run, --check, --json, --save-plan FILE, --plan FILE, --prune | 10.4 |
| Configuration | muructl config export | Export desired state. Flags: --app, --split, --full, --all-releases, --resolve-rollouts, --redact, --param, --format | 10.4 |
| Configuration | muructl config diff -f FILE | Compare a file with desired state | 10.4 |
| Configuration | muructl config show RELEASE --effective | Merged settings with their sources | 3.3 |
| Configuration | muructl config schema | Print the JSON Schema for configuration files | 10.3 |
| Applications | muructl app create, list, show, delete | Manage applications | 3.1 |
| Applications | muructl release create, list, show, delete, wait | Manage releases and wait for states | 9.1 |
| Applications | muructl deploy APP PATH or IMAGE | Create a release and apply the default strategy. Flags: --entrypoint, --interface, --python, --processes, --threads, --sync-threads, --concurrency, --preload, --hooks, --factory, --release, --no-wait, --retain | 9.2 |
| Applications | muructl reload APP | New release from the current source | 9.8 |
| Traffic | muructl host create, list, show, delete | Manage virtual hosts | 4.3 |
| Traffic | muructl route set, list, show, delete | Manage routes and traffic splits | 4.5 |
| Traffic | muructl upstream create, list, show, delete | Manage HTTP upstreams | 4.13 |
| Traffic | muructl rollout canary, bluegreen, status, pause, resume, promote, abort | Run and control rollouts | 9.2, 9.3 |
| Traffic | muructl drain RELEASE | Drain a release | 9.5 |
| Traffic | muructl rollback APP | Restore the previous traffic policy | 9.6 |
| Traffic | muructl maintenance on, off APP | Maintenance page at the edge | 4.12 |
| Operations | muructl dashboard login | A single-use link signing a browser in to the dashboard's actions | 10.7 |
| Capacity | muructl scale RELEASE or APP | Processes, threads, bounds, temporary floors with --until | 7.4, 7.5 |
| Capacity | muructl scale status RELEASE | Signals, decisions, churn counters | 7.3 |
| Capacity | muructl schedule add, list, remove | Scheduled capacity | 7.5 |
| Capacity | muructl edge pools, set | Edge pools and process counts | 4.2 |
| Observability | muructl watch | Live view of applications, releases and workers | 10.7 |
| Observability | muructl events | Show or follow events with filters. Flags: --follow, --after | 11.5 |
| Observability | muructl logs RELEASE | Show or follow logs | 11.2 |
| Observability | muructl logs reopen [--workers] | Reopen log files after external rotation | 11.3 |
| Observability | muructl metrics | Print the node's metrics as Prometheus text | 11.1 |
| Observability | muructl logging level RELEASE | Time-limited logger levels, or --reset | 11.4 |
| Observability | muructl debug stacks TARGET | Python stacks of every thread with the request each is serving, merged across workers. Flags: --worker | 11.7 |
| Observability | muructl debug tasks TARGET | asyncio task tree and loop lag of ASGI workers | 11.7 |
| Observability | muructl debug profile TARGET --for | Sampling profile written as collapsed stacks or pstats. Flags: --worker, --requests, --path, --format | 11.7 |
| Observability | muructl debug trace TARGET --for | In-process sys.monitoring trace. Flags: --worker, --requests, --path, --force | 11.7 |
| Observability | muructl debug memory TARGET | Resident size, gc counts and object counts; --trace --for for allocation sites, --diff for a snapshot comparison | 11.7 |
| Observability | muructl debug attach TARGET --worker N | pdb in a worker over the admin API; --dap for debuggers speaking the Debug Adapter Protocol | 11.7 |
| Observability | muructl debug exec TARGET --worker N -- CODE | Run a snippet in a worker (admin, audited, off in production by default) | 11.7 |
| Observability | muructl debug inspect TARGET | sys.path, modules, redacted environment, settings, descriptors and threads | 11.7 |
| Images and tools | muructl image check IMAGE | Probe an image against the contract | 8.6 |
| Images and tools | muructl image build, dockerfile | Build an image or print the generated Dockerfile (later) | 8.6 |
| Images and tools | muructl tools install uv | Managed copy of uv | 8.4 |
| Traffic | muructl overlay list, show, preview NAME | Overlays, triggers and effective changes | 9.10 |
| Traffic | muructl overlay activate, end NAME [--for, --until] | Manual activation and ending | 9.10 |
| Traffic | muructl overlay promote NAME | Fold an active overlay into the base configuration | 9.10 |
| Traffic | muructl client-class list, test | Show classes, classify a sample request | 4.14 |
| Traffic | muructl ratelimit list, status | Rate limits and current rejections | 4.15 |
| Observability | muructl debug report | Redacted diagnostic bundle for bug reports | 18.6 |
| Development | muructl serve-static DIR | Serve a directory with no configuration. Flags: --listing, --index, --fallback, --https and port options | 4.9 |
| Daemon | muructl daemon stop --shutdown [--timeout] | Drain every release and stop everything, instead of detaching | 13.3 |
| Configuration | muructl secret set, list, remove | Manage the file secret store; values never appear in output | 10.8 |
| Daemon | muructl token create, list, revoke | Bearer tokens for the admin API with a role and expiry | 10.6 |
