# 13. Operations

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

The same components run in every setting, from a laptop to a production node. Development mode embeds the controller and edge in one process; production runs them as a daemon with separate edge processes.

## 13.1 Development mode

```
muructl run                                        # uses [tool.muru] and [tool.muru.run] in the current project
muructl run shop.wsgi:application --no-reload
muructl run myapp.main:create_app --factory --python 3.13
muructl run ./myproject --processes 2 --threads 8 --bind 127.0.0.1:9000
muructl run app:app --single                       # one process, one thread, no fork, for debuggers
muructl run --build                                # bring .venv up to date with the lock first, then serve
```

- No daemon, root, state store or systemd, and nothing of Muru's written into the project directory (locations in section 13.8; the project's own `.venv` that a build makes is uv's, section 8.2). Controller and edge run as threads of one process, hosts and workers as children. Loopback only, HTTP on port 8080 by default, HTTPS on 8443 when enabled (below). Loopback peers are trusted for the forwarding headers of section 4.7, X-Forwarded-Prefix included, so a development proxy that mounts the application under a path (jupyter-server-proxy, code-server) sets SCRIPT_NAME or root_path with no [proxy] table; the same holds under --allow-public, for loopback peers only. Their headers alone are trusted: a loopback peer keeps a client's connection share and timeouts (section 4.18), since it is as often a browser as a proxy.
- **Development mode is said out loud.** Start-up prints that error pages show tracebacks, nothing is persisted, the run reloads on change and only loopback is served, and the dashboard header says development mode, so nobody mistakes muructl run for a production setup. A --bind address outside loopback is refused with a message naming the daemon as the production path. --allow-public serves it anyway with the development behaviours off (no tracebacks in error pages, the 10 MB body limit of ADR 0008, no HSTS warning, no dashboard unless asked for) and a start-up line saying this is still not the supported way to run in production; it exists for trying Muru behind real clients before the daemon exists, and in containers whose port mapping needs 0.0.0.0.
- The argument is an entrypoint (module:callable) or a project: a directory, or its pyproject.toml or muru.toml, whose [tool.muru] table names the entrypoint (section 6.8). With no argument the project is --chdir or the current directory. A flag wins over the table, the table over the defaults. An entrypoint given on the command line replaces the table's entrypoint together with its factory and factory_phase settings, since those describe the entrypoint; processes, threads, preload and hooks still come from the table. --static PREFIX=DIR adds to or replaces the project's [[tool.muru.static]] entries by prefix, served by the edge as section 4.9 describes. Interface detection from the callable.
- **Run is build, then serve.** The interpreter and environment come from section 8.1 and 8.2 under `build.when`: the active virtualenv or ./.venv is used as it is; a project with a lock and no environment is built once, with `uv sync --frozen` into its `.venv` and a terminal line saying so, so a fresh clone followed by muructl run works; a project with neither gets python3 on PATH and a line saying no environment was found. --build MODE sets the key for one run (auto, always or never), a bare --build means always, and `build.when` in `[tool.muru.build]` or in `muru.local.toml` sets it for a project or a machine. Under auto a change to uv.lock, .python-version or requires-python while running builds before the new release starts, a change to pyproject.toml alone reloads without a build, and an environment that is behind the lock at start is reported with the flag to set; always also builds on every start; never builds nothing. Under --allow-public the first-run build is off, and the message names muructl build. The runtime library is the environment's own muru when it matches muructl's version, otherwise the copy in the user cache on the host's PYTHONPATH, fetched once per interpreter tag (section 8.3), so the project need not depend on muru and an offline machine that has it installed needs nothing more.
- **Reload is on.** A development run on loopback watches the project and starts a new release when a source file changes (section 9.8), as runserver and flask run do, without a flag. --no-reload turns it off for one run, reload = false in the run table or the local file for a project or a machine, and --reload turns it back on over a file that turned it off. --allow-public is not development mode, so reload is off there unless --reload is given; containers and production are muructl serve and the daemon, where it never applies. The start-up announcement says the run reloads on change, beside the line naming the directories and patterns watched.
- **The run table and the local file.** `[tool.muru.run]` in pyproject.toml, which is `[run]` at the top level of muru.toml, holds the settings only muructl run has and that describe how this checkout serves rather than what the application is: a key of the table is a flag of the command, in snake case with the same value syntax (port, https, https_only, https_port, tls_cert, tls_key, hosts, reload, reload_dirs, reload_include, reload_exclude, access_log, access_log_exclude, slow, dashboard and compression, the last one the only key that does not mirror its flag, since compression = false reads better than no_compression = true). Left out on purpose: --bind and --allow-public (serving beyond loopback is a conscious act on each command line), --python and --chdir (the machine and the invocation), --single and the link flags (debugger and experiment switches), and --max-body-size (it has a production counterpart in the host limits). The daemon ignores the table, and so does muructl serve, which has its own. `muru.local.toml` beside the project's file holds the `[run]` table, and a `[build]` table whose `when` key development reads (section 8.2), for one developer's machine: a key outside them is refused naming the committed file, it is never committed (muructl says once per run that it is using the file, and that the project's .gitignore does not cover it when that is so), and muructl deploy does not read it. The layering, highest first: flags, muru.local.toml, `[tool.muru.run]`, defaults; lists add across the layers as a list flag adds to a key, and the booleans the file turns on have --no-reload, --no-https and --no-https-only to turn them off for one run. The table applies whenever the project is found, with an entrypoint on the command line too.
- Defaults: one worker with a few threads for WSGI, one event loop for ASGI with a sync pool of 8 threads (--sync-threads) and 1000 requests in flight (--concurrency), preload auto as in the daemon (so an import that is not fork-safe is reported on the laptop, not in production), wsgi.multiprocess False for single-process runs, 100 MB body limit (--max-body-size changes it; the limit in force is muru.max_body_size in the environ).
- Terminal output shows tagged application output and, by default, compact access log lines. Application lines have no time and access lines start with a short local clock; --timestamps puts the full UTC time in front of every line (section 11.8). --access-log says which requests the access log writes, as `access_log` does under muructl serve and the daemon: on (default), errors (only 4xx and 5xx responses and requests slower than --slow, default 1 s) or off; --access-log-file PATH writes the lines there as JSON instead of to the terminal, so errors alone can go to a file. The run table's `access_log` and `access_log_file` are the same, the file relative to the project. --access-log-exclude PATTERN hides matching paths, for example static assets or health probes (probes are already excluded). Whatever is shown, the access log is still kept in memory and available through muructl logs --access and the dashboard. The dashboard and admin socket are available on loopback: the run serves the admin protocol on a socket of its own, so muructl debug (exec and attach included, with no debug_exec needed, since the person at the terminal started the workers), metrics, logs --access, logging, websockets, status and restart work against it unchanged, the project's commands run through it (section 9.12), and its dashboard has the workers' status, stacks, tasks and memory pages; the commands that manage releases, hosts, routes, overlays, configuration, secrets or the daemon say they need the daemon, and events are printed to the terminal rather than kept.
- --print-config prints the settings the run would use and exits: the application's settings a flag can set, then the `[run]` table, each with the layer it came from (the command line, muru.local.toml, the project's run table or its `[tool.muru]` table, or the default), in the shape of the project's file, as muructl serve's does (section 13.9, ADR 0051). The tables no flag changes are named and left to the file. A list names every layer that adds to it. The interpreter is found as the run would find it and named at the end, so one the run could not use is refused here in the same words. A run is not converted to the daemon's format: its flags are about ports, reload and certificates, which are not a release's. muructl render writes a release's table from the settings a deploy takes (section 10.4, ADR 0052), and a release's table needs only its `source` when the project's file says the rest.

**HTTPS in development.** TLS works in development mode with the same edge code as production, which matters for testing Secure cookies, HTTP/2 (browsers only use it over TLS), WebSockets over wss and anything that checks wsgi.url_scheme.

```
muructl run app:app --https                               # HTTP on 8080 and HTTPS on 8443
muructl run app:app --https-only                          # HTTPS on 8443, HTTP redirects to it
muructl run app:app --tls-cert cert.pem --tls-key key.pem # your own certificate
muructl run app:app --https --host shop.localhost         # extra names on the certificate
```

- **Certificates:** with --https and no certificate given, muructl creates one for localhost, 127.0.0.1, ::1 and any --host names. If a local development certificate authority created by mkcert is installed (found through CAROOT in the environment, then mkcert -CAROOT, then mkcert's default location), the certificate is issued from it, so browsers trust it without warnings. Otherwise a self-signed certificate is created and muructl prints how to trust it or install mkcert. Generated certificates and keys are kept in the user cache directory (section 13.8) as certs/dev.pem and certs/dev-key.pem, never in the project, and are reused until they near expiry (30 days before, of a one-year validity), the authority changes or a name is asked for that they do not cover. ECDSA P-256 keys, signed with the ring provider (ADR 0024).
- **Redirects:** with --https-only, the HTTP listener answers every request with 307 and the same path and query on the HTTPS port, taking the host name from the request. 307 keeps the method and is not cached, so a later run without TLS on the same port is unaffected.
- **Ports:** HTTP 8080 and HTTPS 8443 by default on 127.0.0.1, changed with --port and --https-port, with --bind taking the address (an IP, or IP:PORT as a shorthand that --port overrides). If a port is in use, muructl run fails with a message naming the port and the process holding it where it can determine that (through lsof or ss). --port auto picks free ports and prints them; it covers the HTTPS port too unless --https-port says otherwise.
- **Headers:** Muru adds no HSTS header in development, because HSTS on localhost would affect every other local server. The development page warns if the application itself sends one.

```mermaid
flowchart LR
  B[Browser] --> E
  subgraph P [muructl run]
    E[Edge] --- C[Controller]
    FW[File watcher] -->|change| C
  end
  E -->|worker link| W1[Current release workers]
  C -->|new release from same path| H2[New host]
  H2 -.-> W2[New workers]
  E -.->|switch after warm-up| W2
```

## 13.2 From one application to many

| Level | Commands | What you get |
| --- | --- | --- |
| Development | muructl run | ephemeral, reloads on change, settings in the project's run table |
| One long-running application | muructl up, muructl deploy APP PATH | daemon, state, zero-downtime deploys |
| Several applications | muructl apply -f config/ | virtual hosts, TLS, per-application users |
| Managed rollouts | muructl rollout canary ... | canaries, gates, A/B |

## 13.3 Running under systemd

```
[Unit]
Description=Muru
After=network-online.target

[Service]
Type=notify
ExecStart=/usr/bin/muructl daemon run --system
ExecReload=/usr/bin/muructl daemon reload
ExecStop=/usr/bin/muructl daemon stop --shutdown --timeout 60s
TimeoutStopSec=90
ConfigurationDirectory=muru
StateDirectory=muru
CacheDirectory=muru
LogsDirectory=muru
RuntimeDirectory=muru
RuntimeDirectoryPreserve=yes
KillMode=process
Restart=on-failure
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
```

The unit runs the daemon as root, and the daemon keeps root for itself alone: the edge and the applications that name no user run as the service user (ADR 0026), `muru` unless `--user` or the `[daemon]` table names another, which the packages create through sysusers.d and a hand installation creates before the first start; the daemon refuses to start without it. The directory settings let systemd create and own the locations (section 13.8), which the daemon reads from the environment, so no paths are hard-coded. KillMode=process keeps hosts running if systemd restarts the daemon, so they can be re-adopted, and RuntimeDirectoryPreserve=yes keeps their control sockets in place across that restart. The daemon notifies readiness with sd_notify. Without systemd, muructl daemon start, stop, restart and status manage a background daemon.

**Stopping the daemon.** muructl daemon stop (and systemd's stop) detaches by default: the daemon exits but hosts, workers and edges keep serving, and the next daemon start re-adopts them, so a daemon restart or upgrade never interrupts traffic. muructl daemon stop --shutdown drains every release, stops edges and hosts, then exits, for taking a node out of service or before a reboot. A full shutdown removes the host and edge records, the release directories it leaves empty and the rate limits' shared buckets from the runtime directory, so nothing is left to adopt. A daemon started in the background writes RUNTIME/daemon.pid before its admin socket answers and removes it when it exits, whether it detaches or shuts down; an upgrade (section 13.4) is the same process and keeps it. On a system shutdown, systemd stops the unit; the ExecStop line in the unit above gives applications a bounded drain before remaining processes are killed. Whether it detaches or shuts down, the daemon stops the commands running under it as when their callers go away (section 9.12), within the shutdown's budget, and waits for them, so each caller is told how its command ended.

**What SIGTERM means.** SIGTERM and SIGINT detach the daemon too, which is right under systemd, where KillMode=process and a restart must not drop traffic, and wrong as a container's main process: when PID 1 exits the kernel kills every other process in the PID namespace, so a detach there kills the edge and every worker mid-request. The daemon file's `[shutdown]` table says which, so the systemd case stays explicit and the behaviour is testable outside a container:

```toml
[shutdown]
on_sigterm = 'shutdown'   # or 'detach'; shutdown when the daemon is PID 1 or under the init of section 6.10, detach otherwise
delay = '0s'              # keep accepting after the signal, for load balancers to notice
budget = '8s'             # the whole shutdown, under docker stop's ten seconds
```

The shutdown is one sequence, whether SIGTERM under `on_sigterm = 'shutdown'` or muructl daemon stop --shutdown starts it, and it fits the budget: for the delay the edge keeps accepting and serving, with every connection closing after the response in flight (`Connection: close`, GOAWAY on HTTP/2) and at once when idle, so clients with keep-alive connections reconnect, to another node when a load balancer is in front (the default of zero suits Docker and Compose); then the edge stops accepting and the requests in flight finish, within what the budget leaves after a moment for the workers; then every host is stopped at once, each given its workers' stop budget or what remains of the shutdown's, whichever ends first, so the workers close their WebSockets with 1001 and stop; then the edge, last, so those close frames get through it; and the daemon exits 0. Whatever has not finished by the deadline is killed, a host's workers with it. The table is applied on reload. `muructl run` as a container's main process bounds its stop by the same default budget: the drain of what is in flight, then the host with what is left.

## 13.4 Upgrading Muru itself

```mermaid
sequenceDiagram
  participant S as Daemon
  participant O as Old edge
  participant N as New edge
  participant C as Clients
  S->>N: spawn from own image with inherited listener sockets
  N-->>S: ready, snapshot loaded
  S->>O: stop accepting
  C->>N: new connections
  O->>C: finish in-flight, GOAWAY, close idle keep-alives
  O-->>S: exited
```

- Installing a new package changes nothing until muructl daemon upgrade. The daemon then re-execs from the new binary (keeping its pid, children and listener sockets), reloads state, re-adopts hosts and replaces the edges with the handoff above.
- The daemon re-execs from the binary its command name finds on the PATH, or the path it was started with, after checking that it runs: what the kernel reports as the running executable may be a file the installation replaced. The listener sockets come back from the old edge over its listener-return socket, as they do for a restarted daemon, and the old edge is told to stop only once the new one has the snapshot. A connection the old edge accepted whose first request has not yet arrived when it stops is given the header read timeout for that request, which is answered with the connection then closed, since a client has no response to retry on for a connection that is closed before its first request; an idle keep-alive connection, which has had a response, is closed at once. Each edge's control and listener-return sockets are numbered by generation, so a replacement never collides with the edge it replaces. Connections handed over to WebSockets on the old edge end when it exits.
- Runtime library upgrades take effect for new releases. Old releases keep their runtime within the protocol compatibility window.

## 13.5 Backups

muructl daemon backup writes the state store (desired state, release records, rollout history, events and audit log). Configuration exports are the recommended way to keep desired state in version control; backups preserve history.

## 13.6 Large monoliths

- Warm-up gating keeps slow-starting releases out of traffic until ready, with per-release startup timeouts.
- Two releases overlap during deploys, roughly doubling memory. The daemon checks the node memory budget before starting a release and refuses or queues the deploy instead of letting the node swap.
- Workers start in staggered batches. gc.freeze() and preload keep memory shared.
- Cgroups or containers give per-release limits.
- **Details fixed by the implementation.** A release is checked against the memory budget before it starts for a deploy, a rollout or promotion of a release that is not running, and a traffic policy or warm-up that names one; the daemon's own start-up, a restart after a crash and a rollback are not checked. Its memory is its host and its workers at their starting counts, each worker at what the application's serving release measures, else the soft memory limit, else the mean of the node's workers, and the host at what the serving host measures, else a worker's; a managed server, and a release of which nothing is known, are not checked. When it fits only with the retained releases gone, they are stopped, the one created first first, until it fits; when it does not fit even then, the deploy is refused, with no release recorded, and `scaling.denied_by_budget` is raised. Deploys are not queued. `muructl apply` warns of a release that would not fit.

## 13.7 Beyond one node

Multi-node operation is a later phase: the supervisor role becomes a per-node daemon, a controller manages several nodes, and edges run per node behind an L4 load balancer or centrally. Worker endpoints already sit behind an address abstraction, so Unix sockets can become TCP with mTLS.

## 13.8 File locations

Muru has no fixed paths. Each location is resolved at start-up from the first of these that applies:

1. Explicit options: --config, --state-dir, --run-dir, --log-dir and --cache-dir, or the matching keys in the configuration file. The daemon's own commands take all of them (the muructl daemon commands, so that one set of flags starts, stops and reloads a daemon, muructl up and muructl mcp serve --up), as does muructl secret, which reads the daemon's file. A command that only talks to a running instance reads none of the daemon's files, and takes --socket, --run-dir and --system alone (ADR 0051).
2. Environment variables: MURU_DAEMON_CONFIG, MURU_STATE_DIR, MURU_RUN_DIR, MURU_LOG_DIR and MURU_CACHE_DIR. The first carries the mode in its name, as MURU_SERVE_CONFIG does, since a variable has no command around it to say whose file it names. A daemon started with its earlier name, MURU_CONFIG, set and the new one not is refused, since it would otherwise start without its configuration and say nothing.
3. Directories provided by systemd: when the unit sets ConfigurationDirectory=, StateDirectory=, RuntimeDirectory=, LogsDirectory= or CacheDirectory=, systemd creates them with the right ownership and passes their paths in environment variables, which the daemon uses. The unit file then decides the locations.
4. Defaults for the mode the process runs in.

| Location | Holds | System mode (root, or --system) | User mode, Linux | User mode, macOS | Development (muructl run) |
| --- | --- | --- | --- | --- | --- |
| Config | daemon configuration file, and relative paths in it such as lists | /etc/muru/muru.toml | ~/.config/muru/muru.toml | ~/Library/Application Support/muru/muru.toml | none, configuration from flags and the project's [tool.muru] |
| State | state store, built environments (envs/APP/RELEASE), redirected bytecode, runtime library directories per interpreter tag (runtime/TAG), managed tools, uv's Python installs (python) and per-user uv caches | /var/lib/muru | ~/.local/state/muru | ~/Library/Application Support/muru | in memory, or --state-dir |
| Runtime | admin socket, per-release run directories with control and worker sockets | /run/muru | $XDG_RUNTIME_DIR/muru | /tmp/muru-UID (short, for the 104-byte socket path limit) | a per-run directory under the user runtime location, removed on exit |
| Logs | log files when file sinks are configured | /var/log/muru, or journald | ~/.local/state/muru/logs | ~/Library/Logs/muru | terminal, or --log-dir |
| Cache | re-creatable files: development certificates, and in development the runtime library directories per interpreter tag (runtime/TAG) and a managed uv | /var/cache/muru | ~/.cache/muru | ~/Library/Caches/muru | the user cache location |

Linux user-mode defaults follow the XDG base directory conventions (XDG_CONFIG_HOME, XDG_STATE_HOME, XDG_RUNTIME_DIR and XDG_CACHE_HOME are honoured when set). Paths elsewhere in this specification that sit under these locations are relative to them, for example STATE/pycache or RUNTIME/admin.sock.

**Development runs.** muructl run uses neither the system nor the daemon's user locations and writes nothing of Muru's into the project directory. Sockets and run directories go in a temporary per-run directory, state stays in memory unless --state-dir is given, logs go to the terminal, bytecode uses the normal `__pycache__` beside the source, and the application's own virtualenv is used as is. The one thing a run may create in the project is its `.venv`, when the project has a lock and no environment (section 8.2): that is the project's environment, made with the developer's own uv exactly as `uv sync` would make it, not a Muru file. Only re-creatable artifacts shared across projects go in the user cache: the runtime library per interpreter tag, development certificates, a managed uv. The developer's own uv cache and Python installs stay where uv keeps them; muructl does not redirect them in development. Several development runs, in different projects, can run at the same time without a daemon.

**Finding a running instance.** Client commands look for an admin socket in this order: MURU_SOCKET (or --socket), a muructl serve's socket in the run directory (MURU_RUN_DIR, else /run/muru, section 13.9), a development run started from the current project directory, the user daemon's socket when it listens, then the one development run listening; with several runs and no daemon the command names them, so one can be chosen. Each run keeps its socket under RUNTIME/runs/PORT, named by the port the application is served on, beside a run.json that names its pid, application, project directory and address, which a command reads to find it and removes once the pid is gone. muructl status reports which one it connected to, and muructl run prints its socket and dashboard address at start-up.

**Visibility.** muructl daemon run --print-paths, muructl status and just doctor show every resolved location and which rule chose it.

## 13.9 One application in a container

```
muructl serve                                              # [tool.muru] and its serve table in the working directory
muructl serve shop.wsgi:application --listen 0.0.0.0:8080
docker run -e MURU_SERVE_PROCESSES=4 -e MURU_SERVE_LOG_FORMAT=json registry.example.com/shop
```

muructl serve runs one application as one release in one container, configured from the image and the environment, with nothing persistent and nothing administered from outside (ADR 0044). It is how Muru runs under Docker, Compose, Podman and in a Kubernetes pod, where the orchestrator owns rollouts and load balancing and Muru makes the container behave well.

- **One process, one release.** The controller and edge run as threads of one muructl process and the host and workers as its children, as in muructl run (section 13.1); a separate edge process gives no isolation or independent upgrade inside one container. The container is the release: no blue/green, canaries, previews or overlays inside it, since the orchestrator replaces containers. Everything else applies: preload with the import check, worker groups and mounts, timeouts, memory policies, in-pod scaling, static files and uploads at the edge, client classes and rate limits.
- **Production defaults.** It listens on 0.0.0.0:8080 by default, since a container's address is reached only through the runtime's port mapping or a Service. Reload is off, error pages carry no tracebacks, the body limit is 10 MB (ADR 0008) and nothing is built at start. Forwarding headers are applied from loopback peers alone unless `--trusted-proxy` names the ingress or proxy in front (section 4.7). `--processes auto`, the default, comes from the container's limits (below).
- **Sizing from the container's limits.** A server that sizes itself from the node's cores starts far too many workers in a pod limited to 2 CPUs on a 64-core node, and is OOM-killed. On Linux with cgroup v2, serve reads its own cgroup at start (the v2 entry of `/proc/self/cgroup`, under `/sys/fs/cgroup`). `--processes auto` is the quota of `cpu.max` rounded up, at least 1 and at most the CPUs of `cpuset.cpus.effective`, or those CPUs when there is no quota; the edge's event-loop threads follow the quota already. With a limit in `memory.max`, the container's memory budget is the limit less headroom for muructl, the edge and the page cache, 10 % of it and at least 128 MiB. Unless the memory policy sets a hard limit (section 7.7), each worker's hard limit is derived from the budget: what the host holds after preload is taken off, and the rest is divided by the workers of every group. A worker that grows is so replaced with the usual drain before the kernel's OOM killer picks a victim in the cgroup, which may be the host. A worker the kernel kills anyway, seen as a SIGKILL while `memory.events` counts another `oom_kill`, is reported as `worker.oom_killed` and counted in `muru_worker_exits_total` with reason `oom_killed`. Start-up lines say what was derived and from which file, and `--print-config` shows the limits read. Without cgroup v2 limits (macOS, cgroup v1, a container with no limits) the CPU count applies and no memory limit is derived. The limits are read once, so an in-place pod resize takes effect on the next start.
- **Finding the application.** The entrypoint is the argument, else the `entrypoint` the configuration layers give (below), read from the project in --chdir or the working directory; with none, serve fails naming where it looked. Image labels are not read: a process cannot see its own image's metadata, and they are for the daemon's container releases (section 8.5). The interpreter is the `python` setting, else `VIRTUAL_ENV`, `/app/.venv/bin/python` or python3 on PATH, the image contract's order (section 8.6) without its label. The runtime library is the environment's own muru when it matches muructl's version, else the directory `muructl build --runtime-dir` filled (`/opt/muru` by default, `--runtime-dir` to name another), and is never fetched.
- **Configuration in layers**, lowest first: defaults; `[tool.muru]` with its serve table (`[tool.muru.serve]` in pyproject.toml, `[serve]` in muru.toml), whose keys are the flags in snake case as the run table's are; one `MURU_SERVE_*` variable per setting, the flag's name in upper snake case (`MURU_SERVE_LISTEN`, `MURU_SERVE_PROCESSES`); then flags. That is the order servers and configuration libraries commonly use: the image author bakes defaults into the file and the image's command, and a deployment overrides single settings with the environment or arguments. The prefix keeps the variables apart from the MURU_* runtime context (appendix D.3), where `MURU_LOG_FORMAT` already means Python logging's default. A list in a higher layer replaces the lower one, so a deployment can override it whole. The run table and muru.local.toml are not read. `--print-config` prints the resolved settings in the shape of the project's file, the application's settings and then the `[serve]` table with each of serve's own settings given or defaulted, each with the layer it came from, a variable by its name.
- **A settings file in place of the project's.** `--config FILE`, or `MURU_SERVE_CONFIG`, names a project settings file read in place of the pyproject.toml or muru.toml beside the code, which is then not read at all: a file named pyproject.toml for its `[tool.muru]` table, and any other in the form of a muru.toml, with serve's settings under `[serve]` (ADR 0051). It is how a deployment supplies settings from a mounted file. The file replaces the project's outright and the two are never merged, so it holds everything the project's would, the entrypoint and the declared commands included; one setting is changed over it with a variable or a flag. Relative paths in it are taken from the project directory. A command run for the project (section 9.12) reads the same file from the same variable, so a Job and a Deployment agree.
- **No persistent state.** The state store is in memory and rebuilt from configuration on every start. Everything Muru writes at run time goes under one run directory, `/run/muru` by default (`--run-dir`, `MURU_RUN_DIR`): the admin socket, the release's run directory and sockets, and the upload spool, limited by `spool_quota` as in the daemon's `[limits]` (section 4.11), so a large upload is refused with 507 rather than the pod evicted. A run directory that cannot be written fails the start with a message naming the tmpfs or emptyDir it needs; there is no fallback, since a read-only root seldom has a writable `/tmp` either. serve assumes no passwd entry for its UID and creates files and directories group-writable, so it runs under an arbitrary UID with GID 0, as OpenShift assigns, and under `readOnlyRootFilesystem` with that one volume.
- **No installs at run time.** serve never builds an environment, installs an interpreter or fetches the runtime library, and sets `UV_OFFLINE=1` and `UV_PYTHON_DOWNLOADS=never` for anything it runs. A missing interpreter, package or runtime library fails the start, naming it and the build step that should have provided it (`muructl build --deployment` in the image's build stage, section 8.6). It behaves so always, with no detection of whether it is in a container.
- **Access from inside.** The admin socket in the run directory serves the commands muructl run's socket serves (status, logs, logging, metrics, debug, websockets, restart and the project's commands, section 9.12), so muructl in `docker exec` or `kubectl exec` works against it; the commands that manage releases, hosts, routes, overlays, configuration, secrets or the daemon say they need the daemon. Operations that change the process are only on this socket: the platform's permission to exec into the container is their access control.
- **The admin port.** A listener apart from the application's, on 0.0.0.0:9090 by default (`--admin-listen`, `MURU_SERVE_ADMIN_LISTEN`, `off` to close it), so an Ingress to the application never exposes it and a full thread pool never delays it. Binding it to loopback stops Kubernetes probes, which connect to the pod's address.

| Endpoint | Answers | Used by |
| --- | --- | --- |
| `/livez` | the edge's event loops respond and the host's heartbeats arrive; never depends on a worker being free, so a busy pod is alive and a hung worker is Muru's to replace | a liveness probe, which restarts the container |
| `/readyz` | at least `min_ready` workers (1 by default) ready in every mount, and not shutting down | a readiness probe, which takes the pod out of the Service without a restart |
| `/startupz` | preload finished and the first workers ready | a startup probe, which holds the other two off so a slow import needs no long initial delay |
| `/metrics` | the Prometheus metrics of section 11.1 | scraping, autoscaling and alerts |
| `/status` | a read-only JSON snapshot: the release, workers, groups and derived settings; only with `--admin-status`, since it describes the configuration | a person debugging |

- **Probes without curl.** `muructl probe ready`, `live` and `startup` ask the same questions over a probe socket beside the admin socket in the run directory and exit 0 or 1, for a Docker `HEALTHCHECK` in an image with no curl, with the admin port open or closed.
- **Logs.** All output, the host's, the workers', Muru's own and the access log, passes through one relay to standard output (section 11.8): application and Muru lines with no time, since the container runtime stamps each line, and access lines with their full UTC time. `--log-format` (`MURU_SERVE_LOG_FORMAT`) is `text` by default or `json`, one object per line with a `kind` field and the time in `time`, for a log collector.
- **Stopping.** muructl is the container's PID 1 with the built-in init (section 6.10). SIGTERM runs the shutdown of section 13.3, with `/readyz` failing at once, before the delay, so the kubelet and the endpoints controller stop sending traffic as early as they can: for `shutdown_delay` (0 s by default, as the daemon's) the listener keeps accepting, every connection closing after its response; then the listener closes, the requests in flight finish and the host stops, the whole inside `shutdown_budget` (8 s by default), and the process exits 0. A project command running through the admin socket gets SIGTERM as the shutdown starts and SIGKILL when the budget ends, if it has not ended by then (section 9.12). A second signal ends the delay. Under Kubernetes the delay covers the seconds in which the Service's endpoints and the ingress still route to a terminating pod, so no `preStop` hook is needed; the Downward API does not expose `terminationGracePeriodSeconds`, so the deployment sets the budget under it, as it sets any other setting.
- **One image, two shapes.** An image that follows the image contract (section 8.6) runs under serve and as a daemon's container release (section 8.5) unchanged: the same interpreter lookup, the runtime library under `/opt/muru`, a non-root user and the project's `[tool.muru]` in the working directory.
