# Changelog

All notable changes to Muru are recorded here. The file is assembled from
fragments in `changelog.d/` by towncrier when a release is prepared; do not
edit the released sections by hand.

Muru follows semantic versioning. Before 1.0, minor versions may contain
breaking changes, which are listed under each release.

<!-- towncrier release notes start -->

## 0.1.0a18 (2026-11-05)

### Added

- `muructl logs` with no application, against a development run, shows the
  run's application's output, as the `debug` commands take the run's
  application when none is named. The daemon still needs the application
  named unless `--access` is given, and says so. ([#375](https://github.com/muruhq/muru/issues/375))
- `muructl debug inspect` hides a password carried in a URL, whatever the
  variable's name: `DATABASE_URL=postgres://shop:hunter2@db/shop` is shown
  as `postgres://shop:***@db/shop`, and the rest of the URL is kept. A
  name ending in `_DSN` is hidden whole, as those with SECRET, TOKEN,
  PASSWORD, KEY, CREDENTIAL or AUTH in them are. ([#376](https://github.com/muruhq/muru/issues/376))
- `muructl requests` shows the requests in flight now as `muructl websockets`
  shows open sockets: counted by worker group, since the edge queues per
  group, as QUEUED and QUEUED-OLDEST, THREADS (busy of the group's threads)
  and THREAD-OLDEST, and LOOP and LOOP-OLDEST, the columns of the sides the
  release has; and with `--list` each request, the oldest first, with its age,
  state (queued or running), kind (`wsgi`, `wsgi streaming`, `asgi`, `asgi
  streaming`, `grpc`, `handoff` or `upstream`), worker, method and target,
  client and request id. Served by the daemon, a development run and `muructl
  serve`, with `--json` and an MCP tool beside `websockets`. The edge keeps
  the listing beside each request's access log record, spread over several
  maps so that a request's arrival and end never queue on one lock, and a busy
  worker's throughput measured the same with it. ([#379](https://github.com/muruhq/muru/issues/379))

### Changed

- `muructl debug status` shows what each side of a worker holds as of the
  moment it answers: THREADS and THREAD-OLDEST for the application threads,
  counting a request held at `breakpoint()` or streaming its response, and
  LOOP and LOOP-OLDEST for the event loop, open WebSockets left out, in place
  of one IN-FLIGHT count and an OLDEST age that could be a second old and left
  exempt requests out. A worker that does not answer within a second is shown
  from its last report, marked with a star. STATE is `starting`, `ready`,
  `debugging`, `replacing` or `retiring`, ROUTED says whether the edge sends
  it requests, and `--json` carries `state`, `routed`, `threads`, `loop` and
  `stale` in place of `replaced`, `in_flight` and `oldest_ms`. The dashboard's
  workers page shows the same. Every header of a `muructl` table is one word
  now: `QUEUE-P95` in `scale status` and `REQUEST-ID` in `websockets --list`. ([#369](https://github.com/muruhq/muru/issues/369))

### Fixed

- When the edge discards the rest of a worker's response, after an
  internal redirect, an error page in its place or a client that went
  away, it no longer reads each body frame whole into memory first. A
  drained 16 MiB response no longer costs the edge a 16 MiB allocation. ([#346](https://github.com/muruhq/muru/issues/346))
- The note muructl prints when it provides the runtime library now says where
  the wheel came from: `unpacked the runtime library FILE from DIR` for a
  wheel directory, and `fetched the runtime library FILE from PyPI` for a
  download. Before, both said "fetched". `muructl runtime install` and
  `muructl build --runtime-dir` say the same. ([#354](https://github.com/muruhq/muru/issues/354))
- `muructl daemon stop --shutdown` no longer leaves host and edge records,
  empty release directories and the rate limits' buckets in the runtime
  directory. A daemon started in the background now writes `daemon.pid`
  itself and removes it when it exits, so the file no longer names a
  daemon that has gone. ([#355](https://github.com/muruhq/muru/issues/355))
- The errors for a key muru.local.toml does not take now read as sentences:
  `processes is not a key of muru.local.toml, which takes only the [run]
  table and the when key of [build]`, and for `[build]`, `whose [build] table
  takes only the when key`. A scaling table with another key beside
  `mode = 'static'` is now refused with `mode = 'static', the default, takes
  no other key but advisory`, which also names the one key it does take. ([#359](https://github.com/muruhq/muru/issues/359))
- A request whose response ended before its body had been read no longer
  leaves the client connection stuck. On HTTP/1.1 the edge now reads and
  discards the rest of the body, up to 1 MiB, before the end of the response
  goes out, and closes the connection after the response when the remainder
  is larger or slow to arrive; a remainder declared over 1 MiB is given up at
  once. Ending the response first had left the connection waiting on both
  sides with clients such as oha. An ASGI worker whose application answered
  without receiving a body larger than it buffers now drains the rest of the
  body once the response is complete, where it had stopped reading from the
  edge and held up the request and the connection. ([#360](https://github.com/muruhq/muru/issues/360))
- A worker under `muructl debug trace` keeps serving while the trace runs,
  so the trace counts the requests it was run to look at. The host had
  taken the traced worker out of routing, which left the trace with only
  the requests already in flight and reported nothing for an idle worker. ([#361](https://github.com/muruhq/muru/issues/361))
- The `pre_fork` hook now runs in the host before the preload import check,
  so a socket or pipe the import left open and `pre_fork` closes is no
  longer reported, as the check's advice to close it in `pre_fork` says.
  `muructl check` runs `on_load` and `pre_fork` in the same places, so it
  reports what the host would. The hook had run only at the first fork,
  after the check, so following that advice never passed it. ([#365](https://github.com/muruhq/muru/issues/365))
- The preload import check names the module whose import left a socket,
  pipe or thread behind when that module was imported with `from package
  import module` or `from . import module`. It had named the module doing
  the importing instead, since importlib loads such a submodule without
  the check seeing it as an import of its own. ([#366](https://github.com/muruhq/muru/issues/366))
- A request the application failed, or that ran past the soft timeout or
  the request deadline, is written to the access log with the time the
  application ran, where it had shown `app=0.0ms` however long it took.
  The JSON access log's `app_ms` and the application time metric get the
  same figure, for worker, gRPC and managed server requests alike. ([#367](https://github.com/muruhq/muru/issues/367))
- An event a worker raises, such as a request past the soft timeout, an
  event loop stall or a recycle, is one line in a development run's
  terminal and one in a release's log under the daemon: the host's line,
  which now starts with the event's kind, as in `request.soft_timeout:
  worker 0 (pid 5121): ...`. The worker had printed its own line as well,
  and `muructl run` printed the event a third time. The start-up lines
  `worker 0 started as pid ...` and `... is ready` are unchanged. ([#368](https://github.com/muruhq/muru/issues/368))
- `muructl debug memory` writes every size with a unit, as `debug status`
  does: `rss 47.4MB  pss 29.8MB  uss 28.5MB`, and the tracemalloc sizes of
  `--snapshot` likewise. pss, uss and the tracemalloc sizes were bare
  numbers of bytes, since they are seldom a whole number of kilobytes. ([#370](https://github.com/muruhq/muru/issues/370))
- `muructl debug inspect` names a free-threaded build once. For a WSGI
  worker the process model is `free-threaded` already, so the build was
  named twice: `(group default, free-threaded, free-threaded)`. An ASGI
  worker's model is `async`, and its line still ends `async,
  free-threaded` on such a build. ([#371](https://github.com/muruhq/muru/issues/371))

### Security

- Secret files (`as = 'file'`) no longer stay on disk after the release
  that needed them: a release's files are removed when its host stops, and
  a command run with the release gets files of its own, removed when it
  ends. ([#357](https://github.com/muruhq/muru/issues/357))

### Documentation

- The daemon guide says how a daemon run as a container's main process is told
  that its image was built ahead of time: `offline = true` and
  `python_downloads = "never"` in `[tools]`, and `never` as the build setting.
  The daemon does not detect a container, since a development container wants
  the builds and downloads a node does. ([#285](https://github.com/muruhq/muru/issues/285))
- The tutorials show the start-up output `muructl run` prints now: the
  runtime library line, the worker's ready line, the current development
  mode line, the admin socket and the directory watched for changes, and a
  reload as it is printed. ([#372](https://github.com/muruhq/muru/issues/372))
- The debugging guide's pdb sample stops on the `breakpoint()` line, as a
  session does on Python 3.13 and later, and the guide says that an IDE
  stops at the line after it. ([#373](https://github.com/muruhq/muru/issues/373))
- The debugging guide and the specification say what `muructl debug profile`
  costs a busy worker, measured in each way the samples are taken: nothing
  measurable at the default interval, read from outside or in the worker with
  or without the GIL, and a few percent at a 1 ms interval. ([#374](https://github.com/muruhq/muru/issues/374))


## 0.1.0a17 (2026-11-04)

### Added

- Added `access_log`, `access_log_exclude` and `slow` to the daemon's
  `[logs]` table and to an application's table, so the daemon's access log
  can write only errors and slow requests, or nothing, for every
  application or for one. `muructl daemon reload` and `muructl apply` change
  what the running edge writes without a restart, and the reload reports
  log rotation as `[logs] rotation` beside `[logs] access_log`. ([#331](https://github.com/muruhq/muru/issues/331))
- Added each instance of a managed HTTP server to `muructl release show`,
  with its pid and address, and to its JSON as `server_instances`, with
  `index`, `pid`, `address` and `started_unix_ms`, so the server's
  processes can be found without walking the daemon's process tree. ([#333](https://github.com/muruhq/muru/issues/333))
- Added `muructl logging access`, which changes which requests the access
  log writes (`on`, `errors` or `off`) for a period, for one application
  or every one and for some paths or all, and goes back on its own; it
  lists the changes in force and `--reset` puts them back. It works
  against the daemon, a development run and `muructl serve`, and the MCP
  server offers it as the `access_log` tool. ([#334](https://github.com/muruhq/muru/issues/334))

### Changed

- Changed `muructl run`'s and `muructl serve-static`'s `--access-log` to
  mean what it means for `muructl serve` and the daemon: which requests
  the access log writes, `on` (the default), `errors` or `off`. A file is
  now named with `--access-log-file PATH`, which writes JSON lines there
  in place of the terminal and can be combined with `--access-log errors`
  to keep only errors and slow requests in a file. The run table's
  `access_log` takes the same values, and `access_log_file` names the file
  relative to the project. `terminal` and a file path are no longer values
  of `--access-log`. ([#343](https://github.com/muruhq/muru/issues/343))

### Fixed

- Fixed `muructl debug stacks` and whole-worker profiles on Linux with
  Python 3.14 showing only each thread's innermost frames: 3.14's reader
  from outside stops where C code called into Python, so a request stopped
  at `breakpoint()`, or in a callback from C, lost the application's
  frames. On 3.14 the worker is now asked, as on macOS, and read from
  outside only when it does not answer in time. ([#306](https://github.com/muruhq/muru/issues/306))
- Fixed requests to an upstream or a managed HTTP server failing with 502
  `upstream_failed` when the server closed a kept connection just as the
  request went out on it, which servers such as Apache do routinely. A
  GET, HEAD, OPTIONS, TRACE, PUT or DELETE request whose body is empty or
  held whole is now sent once more on a new connection, and is marked as a
  retry. Kept connections are also used for no longer than the server's
  `Keep-Alive: timeout` allows. ([#329](https://github.com/muruhq/muru/issues/329))
- Fixed two things a script reading `muructl --json` tripped over:
  `muructl status` gave the number of applications and releases as `apps`
  and `releases`, the names other commands use for lists, which are now
  `app_count` and `release_count`; and `muructl apply` reported a release
  that failed to start with the outcome `failed` alone, which now carries
  the reason, as `release show` gives it. ([#332](https://github.com/muruhq/muru/issues/332))
- Fixed the access log holding up requests: each line was written by the
  request's own task, so a slow disk or a reader of standard error that
  had stopped, such as a full pipe to a container's log collector, stalled
  serving. Lines now go to a writer thread of their own in batches, and
  when it falls far behind they are dropped and counted in the new
  `muru_access_log_dropped_total` metric instead. ([#335](https://github.com/muruhq/muru/issues/335))
- Fixed a worker's thread staying taken for good when a client went away
  while the edge was sending a file the application had handed over with
  `wsgi.file_wrapper` or the ASGI `pathsend` and `zerocopysend`
  extensions: enough such clients made the release answer every request
  with 503 `queue_full` until it was replaced. ([#336](https://github.com/muruhq/muru/issues/336))
- Fixed static files costing the edge more than they should. Each request
  handed work to the blocking pool three or more times (to find the file,
  once per precompressed variant looked for, to open it and once per 64 KiB
  read, through a second copy), so a small file cost the edge more than an
  application serving it, and a route with a fallback application sent
  large files more slowly. A file is now found, with its variants, in one
  call and opened and read in another, in chunks of 256 KiB, and a file no
  larger than a chunk is sent whole: 1 MiB files cost the edge half the CPU
  and small ones about a third less. ([#337](https://github.com/muruhq/muru/issues/337))
- Fixed offloaded uploads holding up every connection through the edge when
  the spool's disk was slow: each piece of a body was handed to the blocking
  pool on its own, and the spool directory was checked and each request's
  files closed and removed on the threads that serve connections. A file is
  now written in blocks of 256 KiB, each added to its digests with its write
  on the blocking pool, and the rest of the file system work goes there too. ([#338](https://github.com/muruhq/muru/issues/338))
- Fixed large bodies crossing the worker link slowly on macOS, where a Unix
  socket's send buffer starts at 8 KiB, so a response was carried in 8 KiB
  steps with a wakeup on both sides for each. Both ends of the link now ask
  for a 256 KiB send buffer, which on macOS about doubles the requests per
  second for bodies of 64 KiB and up and halves the worker's CPU per
  request. ([#340](https://github.com/muruhq/muru/issues/340))
- Fixed both ends of the worker link filling every frame's buffer with
  zeros before reading the frame into it, a second pass over every byte of
  a large body. ([#341](https://github.com/muruhq/muru/issues/341))
- Fixed the edge holding a large piece of a response body until all of it
  had arrived from the worker before sending any of it to the client. A
  piece of more than 64 KiB is now passed on as it arrives, so the edge
  writes to the client while the worker is still sending. ([#342](https://github.com/muruhq/muru/issues/342))
- Fixed the daemon ignoring `MURU_WHEEL_DIR`: with no `wheel_dir` in its
  `[tools]` table it now takes the runtime library's wheels from the
  directory the variable names, as `muructl run` does, so a prepared
  environment such as a container image or a devcontainer needs no daemon
  configuration file for it. `wheel_dir` in the file wins over the
  variable. ([#344](https://github.com/muruhq/muru/issues/344))
- Fixed static routes reading the whole listing of every directory on a
  request's path to check that each name matches the request's case. The
  cost grew with the directory, so a file in a directory of 5,000 entries
  was served at about a fortieth of the rate of one in a small directory.
  Each name is now checked on its own: macOS reports the name an entry is
  stored under, and on Linux the file systems that keep names as given
  need no further check, a directory that folds case and other file
  systems still being read. ([#347](https://github.com/muruhq/muru/issues/347))
- Fixed files sent with `wsgi.file_wrapper` (and so Django's
  `FileResponse`) costing the edge more than static files. The edge read
  them in 64 KiB pieces, each a separate hand-off and copied twice; it now
  reads them as it reads static files, in 256 KiB pieces, so a 1 MiB file
  costs it half the CPU and is served nearly twice as fast. ([#348](https://github.com/muruhq/muru/issues/348))

### Documentation

- Corrected the daemon's `[tools]` reference and the interpreters guide,
  which said that with `offline = true` builds take their wheels from
  `wheel_dir`. An offline build installs only what the daemon's uv cache
  already holds, and `wheel_dir` is for the runtime library. ([#350](https://github.com/muruhq/muru/issues/350))


## 0.1.0a16 (2026-11-03)

### Added

- Added a `uwsgi` profile for managed HTTP server releases: uWSGI binds
  with `--http-socket` and is given `--master` and a hook that makes TERM
  a graceful stop, which in uWSGI 2.0 would otherwise reload and drop the
  requests in flight. ([#95](https://github.com/muruhq/muru/issues/95))
- Added `max_requests` and `max_lifetime` to a worker group's table, so a
  group's workers can be recycled on a count or a lifetime of their own;
  a group that sets neither takes the release's. ([#150](https://github.com/muruhq/muru/issues/150))
- Added a configuration reference listing every key, with its type and
  default, of the daemon's configuration, applied files, `[tool.muru]`,
  `muru.local.toml` and the `muructl serve` file, and a JSON Schema for each
  file for editors, both generated from the definitions `muructl` parses
  them with. ([#262](https://github.com/muruhq/muru/issues/262))

### Changed

- A zero or an empty value that used to be quietly replaced by the default
  is now refused with a message naming the key: `[compression]`'s
  `min_size = "0"`, `encodings = []` and `types = []`, a route's
  `max_body_size = "0"`, and a maintenance `retry_after` under one second. ([#323](https://github.com/muruhq/muru/issues/323))

### Fixed

- A relative `MURU_STATE_DIR`, `MURU_CACHE_DIR`, `MURU_LOG_DIR` or `MURU_CONFIG`, or the matching `--state-dir`, `--cache-dir`, `--log-dir` or `--config`, is now taken from the directory the command runs in. Before, the runtime library unpacked under it went on the host's `PYTHONPATH` as the relative path, so under the daemon, or `muructl run --chdir`, the host could not import `muru` and exited before its workers were ready. ([#308](https://github.com/muruhq/muru/issues/308))
- On Alpine and other musl-based systems, the runtime library `muructl` fetches or unpacks is now the musllinux wheel. Before, Python 3.11 to 3.13 were taken for glibc and given the manylinux wheel, whose extension cannot load on musl, and Python 3.14 and later found no wheel at all, because `platform.libc_ver()` reports musl's version as `1`. ([#313](https://github.com/muruhq/muru/issues/313))
- When `muructl serve` cannot create its run directory because its user may not, the error now names that user and the Dockerfile line that makes the directory for it, `RUN install -d -o UID -g 0 -m 0770 /run/muru`. Before, it gave the advice for a read-only root filesystem, which did not apply. ([#314](https://github.com/muruhq/muru/issues/314))
- A proxy named with `muructl serve --trusted-proxy` now has the same
  forwarding header types applied as the loopback trust serve has without
  one, `X-Forwarded-Prefix` and `X-Forwarded-Port` included, rather than
  only the four default types of `[proxy]`. ([#324](https://github.com/muruhq/muru/issues/324))

### Documentation

- The container guide installs `muructl` in the build stage with `uv tool install`, which needs no pip, and shows what `muructl build` runs, how `[tool.uv]` settings and `UV_` variables reach it, and the same build with uv alone and `muructl runtime install`. ([#247](https://github.com/muruhq/muru/issues/247))
- The container guide's image makes `/run/muru` for its non-root user, without which `muructl serve` could not start under a plain `docker run`. ([#314](https://github.com/muruhq/muru/issues/314))
- The documentation uses more of a wide screen: its content column grows
  with the window, up to 76em, so wide tables such as the configuration
  reference fit.
- The documentation's home page says what Muru is, why it exists and how it works, including what it offers AI coding agents, with a short start and a guide to the rest of the documentation; the README is a short introduction that points to it.
- The how-to guides are grouped by task, each with a line saying what it covers, and the documentation's sidebar separates using Muru from the project and its design.
- The user guide's pages no longer open with a note of what is built and what is still to come: each describes what Muru does, with any limit stated where it applies, and work still to come is in the roadmap and the issue tracker. The error reference lists the reasons `no_route`, `not_found`, `misdirected`, `sendfile_missing`, `sendfile_refused` and `edge_overloaded`, which it left out.


## 0.1.0a15 (2026-11-02)

### Added

- Added `muructl serve`, which runs one application as a container's main
  process: the edge, controller, host and workers in one process tree,
  production defaults, listening on `0.0.0.0:8080`, with settings from
  `[tool.muru]`, its `[tool.muru.serve]` table, a file named by
  `MURU_SERVE_CONFIG`, one `MURU_SERVE_*` environment variable per flag, and
  the flags, in that order. `--print-config` shows where each setting came
  from. Everything goes to standard output, as text or, with `--log-format
  json`, as JSON objects, and `muructl` commands run inside the container
  reach it through its run directory. ([#102](https://github.com/muruhq/muru/issues/102))
- Added the admin port of `muructl serve`, on `0.0.0.0:9090` by default:
  `/livez`, `/readyz`, `/startupz` and `/metrics` for liveness, readiness
  and startup probes and Prometheus, `/status` with `--admin-status`, and
  `muructl probe ready` (or `live`, `startup`) for a health check in an image
  with no curl. ([#103](https://github.com/muruhq/muru/issues/103))
- Added `--shutdown-delay` and `--shutdown-budget` to `muructl serve`
  (`MURU_SERVE_SHUTDOWN_DELAY`, `MURU_SERVE_SHUTDOWN_BUDGET`). On SIGTERM,
  readiness fails at once. For the delay the listener keeps accepting, with
  every connection closing after its response. The whole stop fits inside
  the budget, so a Kubernetes rolling update loses no requests and needs no
  `preStop` hook. A serve table can now also set `runtime_dir`. ([#104](https://github.com/muruhq/muru/issues/104))
- `muructl serve` now sizes itself from the container's cgroup v2 limits.
  `--processes auto`, the default, is the CPU limit rounded up, not the
  node's cores. With a memory limit, each worker gets a hard limit unless
  one is set: the limit, less headroom and what the host holds after
  preload, divided between the workers. A worker the kernel kills for
  memory anyway is reported as `worker.oom_killed` and counted in
  `muru_worker_exits_total` with the reason `oom_killed`, which `muructl
  run` and `muructl serve` now serve on `/metrics`, beside
  `muru_worker_recycles_total`. ([#105](https://github.com/muruhq/muru/issues/105))
- `muructl serve` runs under an arbitrary user ID with group 0 and a
  read-only root filesystem: everything it writes goes under one run
  directory, `/run/muru` by default, it stops at once naming the volume it
  needs when that cannot be written, and uploads spooled to disk are held to
  `--spool-quota`. ([#106](https://github.com/muruhq/muru/issues/106))
- Under `muructl serve --log-format json`, Python logging writes JSON
  records too. A record that is already a JSON object becomes the line's
  object rather than a string inside another, and a traceback is gathered
  into one record. Every JSON line carries the pod's name, namespace and
  node, and the OpenTelemetry service and resource attributes, when they
  are set. A log collector that splits records at newlines therefore keeps
  each record and each traceback whole. ([#107](https://github.com/muruhq/muru/issues/107))
- Added `muru_capacity_utilisation`: the share of each group's worker
  slots in use, sampled every second and averaged over 30 seconds, for an
  autoscaler such as a Kubernetes HorizontalPodAutoscaler or KEDA. The
  metrics guide shows how to scale on it and on the time requests wait for
  a slot. ([#108](https://github.com/muruhq/muru/issues/108))
- `muructl serve` never builds an environment, installs an interpreter or
  fetches the runtime library: it finds the interpreter in the image and the
  runtime library in the environment or `--runtime-dir` (`/opt/muru` by
  default), and stops naming the build step that should have provided what
  is missing. ([#110](https://github.com/muruhq/muru/issues/110))
- Every Python log record now carries the request it was logged in and
  where it was logged, as `muru_request_id`, `muru_app`, `muru_release`,
  `muru_group` and `muru_worker`, for an application's own format. The
  `json` log format writes the request id as `request_id`. ([#168](https://github.com/muruhq/muru/issues/168))

### Changed

- Log lines now carry a time where it is wanted: every line in the daemon's
  log files, and in output redirected to a file, starts with the full UTC
  time (`2026-11-02T05:42:22.836Z`); `muructl logs` shows it, with
  `--no-timestamps` to leave it off; and `muructl run --timestamps`,
  `muructl daemon run --timestamps`, `timestamps = true` in the run table or
  `[logs] timestamps = true` put it in front of every terminal line. The
  access log's terminal line shows the local clock, the dashboard shows
  times on the browser's clock with the full time on hover, and the daemon's
  own messages read like Muru's other lines, `muru: warning: ...` with the
  event's details in brackets. ([#93](https://github.com/muruhq/muru/issues/93))
- `muructl debug`, `muructl restart` and `muructl logging level` no longer
  need an application against `muructl run` or `muructl serve`, which
  serve only one, so `kubectl exec deploy/shop -- muructl debug stacks`
  works as it is. ([#291](https://github.com/muruhq/muru/issues/291))

### Documentation

- Added a guide to running `muructl serve` as a Kubernetes Deployment:
  probes, a rolling update that loses no requests, a restricted security
  context, sizing from resource limits and resizing, logs for a collector, autoscaling
  with an HPA or KEDA, and migrations as a Job. The container guide gained
  a Compose service. ([#119](https://github.com/muruhq/muru/issues/119))


## 0.1.0a14 (2026-11-01)

### Added

- Added handing a request from a WSGI application to an ASGI one in the
  same worker, enabled with `handoff = "asgi"`: `environ["muru.wsgi_to_asgi"]`
  continues an HTTP request, or a WebSocket upgrade, as an ASGI application,
  and raises `muru.HandoffNotEnabled` without the setting. ([#91](https://github.com/muruhq/muru/issues/91))
- Added commands an application declares in `[tool.muru.commands]`, such as a migration, a backup or creating an account, each a program or a Python callable with checked inputs, including secret ones taken from a secret store. `muructl command list` shows them with each one's last run, `muructl command run` runs one with a release, and coding agents get `commands` and `run_command` MCP tools. ([#96](https://github.com/muruhq/muru/issues/96))
- Added `muructl exec`, which runs a one-off command, such as a database migration, with a release's interpreter, environment, secrets, user and source directory, streaming its output and exiting with its status. A new how-to guide covers database migrations while two releases share a database. ([#122](https://github.com/muruhq/muru/issues/122))
- Added `muructl mcp serve`, a Model Context Protocol server that gives a coding agent the daemon's operations as tools: status, lists, releases, events, logs, worker stacks and the other debug views, metrics, deploy, promote, rollback, drain, rollouts, scaling, restarts, overlays, maintenance, apply and log levels, plus the user guide. Each tool runs the matching `muructl` command with `--json`; `--read-only` and `--allow` limit what the agent may change, and `--up` starts the daemon when none answers. The coding agents guide describes the development loop. ([#197](https://github.com/muruhq/muru/issues/197))
- Added `--json` to every `muructl` command that reports something: status, the lists, releases, rollouts, overlays, scaling, events, logs, metrics, WebSockets, and what deploy, rollback, drain, restart, maintenance and the other changes did. The output is the admin API's answer with its field names, streams print JSON lines, and a failure prints `{"error": ...}` with the same exit status; the JSON output reference lists each command's shape. ([#254](https://github.com/muruhq/muru/issues/254))
- Added `muructl mcp skill`, which prints, installs or checks a skill for coding agents that teaches the development loop under Muru: what releases and traffic are, what deploy, promote, rollback, drain and rollouts do, and what to check after each. The skill is embedded in `muructl` and versioned with it; `install` writes it to `.claude/skills/muru/SKILL.md` (or another directory with `--dir`), `status` says whether the project's copy is current, and the MCP server tells the agent when it is out of date. ([#255](https://github.com/muruhq/muru/issues/255))
- Added handing a request from an ASGI application to a WSGI callable on
  the worker's threads, enabled with `handoff = "wsgi"`, which also lets an
  ASGI mount have `threads` of its own for it. The access log marks a
  request handed across, and `muru_handoffs_total` counts them. ([#273](https://github.com/muruhq/muru/issues/273))
- Added `muru.asgi.register()`, which runs the lifespan of an ASGI
  application that requests are handed to, `muru.loop()`, the worker's
  event loop for WSGI code, and `muru.request_id()`, the id of the request
  being served. ([#274](https://github.com/muruhq/muru/issues/274))
- Added `muru.asgi.websocket.Handler`, an ASGI application that runs a
  WebSocket's connect, message and disconnect events as calls on an object,
  on application threads, so a WSGI application handles WebSockets in
  synchronous code. ([#275](https://github.com/muruhq/muru/issues/275))

### Changed

- Client class and rate limit names follow the same rule as every other name: lowercase letters, digits and hyphens, up to 63 characters, with no hyphen at either end. Names with underscores, such as `ai_crawler`, are refused; rename them, for example to `ai-crawler`. ([#281](https://github.com/muruhq/muru/issues/281))

### Fixed

- Fixed a race in which a request accepted just as the edge began to drain, on Ctrl-C, `docker stop` or an edge replacement, could reach a worker that was already stopping and be answered 502; the drain now waits until the edge has stopped accepting. ([#241](https://github.com/muruhq/muru/issues/241))
- `muructl run` and `muructl check` now refuse the mount, worker group and environment variable names the daemon refuses, and check a group's `match` the same way, so a project that runs in development no longer fails its first deploy over a name such as `async_plain`, and a group named `default` is refused when the project is read rather than when the host starts. ([#280](https://github.com/muruhq/muru/issues/280))
- `muructl run` and `muructl check` now make the daemon's checks of a project's worker groups, mounts, scaling tables, `max_requests` and file hint keys: a group without paths or match, groups asking the same of one path, a group naming a missing mount, a mount path not below `/`, an entrypoint that is not `module:callable`, two mounts at one path or a scaling table that does not resolve fails there as it would fail a deploy. ([#282](https://github.com/muruhq/muru/issues/282))

### Documentation

- Added explanation pages on releases and traffic policies and on how scaling decides, a how-to guide for upgrading Muru, and a reference page for the supported platforms and Python versions. ([#259](https://github.com/muruhq/muru/issues/259))


## 0.1.0a13 (2026-10-31)

### Added

- Added `muructl check`, which loads an application in a throwaway interpreter and reports what a run would fail on: the project's tables, the interpreter against `requires-python`, the runtime library, the entrypoint and its interface, the hooks module and the mounts, and what the import check finds, with `--strict` turning findings into failures and `--env-file` supplying settings the import reads. Nothing is built or served. ([#114](https://github.com/muruhq/muru/issues/114))
- Added version requests for the interpreter: `--python 3.13`, `python = "3.13"` in `[tool.muru]`, a `.python-version` file or `requires-python` name a Python that `muructl run` and `muructl deploy` resolve with uv, and every interpreter is probed before the host starts, so an unsupported one or one outside `requires-python` fails with a message naming both versions. `muructl release show` prints the resolved version and tag, and the daemon's `[tools]` table names uv, where managed interpreters go and whether they may be downloaded. ([#124](https://github.com/muruhq/muru/issues/124))
- The daemon builds a release's environment from `uv.lock` before it starts: a source with a lock and no `.venv` gets one of its own under the state directory, built as the release's user with uv's output in the release log, and the release is `building` meanwhile. `--build`, `--build-extra` and `--build-group` on `muructl deploy`, or the `build` table of a project or a declared release, say when and how; a Python the node lacks is downloaded first when the `[tools]` table allows it. ([#125](https://github.com/muruhq/muru/issues/125))
- Daemon-managed releases write their bytecode to a directory of their own under the state directory instead of the source tree, so an immutable or read-only release directory still gets a cache and cold starts stay fast; a source that already holds compiled bytecode for the interpreter is used as it is. The `bytecode` setting, `redirect`, `source` or `off`, is in the project's `[tool.muru]` table, a declared release or `--bytecode` on `muructl deploy`. ([#126](https://github.com/muruhq/muru/issues/126))
- The `muru` runtime library is provided by `muructl` itself and no longer has to be a dependency of the application: `muructl run` and the daemon put the copy matching their own version on the host's `PYTHONPATH`, fetching the wheel for the interpreter once into the user cache or the state directory. An environment that already has the matching `muru` is used as it is, so offline machines need no flag; `MURU_WHEEL_DIR` and `MURU_OFFLINE`, or `wheel_dir` and `offline` in the daemon's `[tools]` table, cover machines without network. `muructl runtime install` unpacks the library for an image. ([#127](https://github.com/muruhq/muru/issues/127))
- Added the metrics the design named and the daemon did not yet export: `muru_gil_wait_seconds` and `muru_loop_lag_seconds` histograms for each worker, `muru_worker_recycles_total` by reason and `muru_worker_exits_total` by reason, `muru_async_slots_busy` for requests held on ASGI event loops, and `muru_tls_handshakes_total` by listener and result. The metrics guide now lists every family served, including the autoscaler's `muru_workers_desired`, `muru_scaling_decisions_total` and `muru_churn_reaps_total`, and a test keeps the guide and the exposition in step. ([#138](https://github.com/muruhq/muru/issues/138))
- Each worker now times its garbage collections, by generation and by whether requests were in flight, and reports them with its CPU time and GIL waits: `muru_gc_collections_total`, `muru_gc_seconds_total`, `muru_gc_collected_objects_total` and the `muru_gc_pause_seconds` histogram in the metrics, a line per generation in `muructl debug memory` and the dashboard's worker view, a `gc` share in `muructl scale status`, and a diagnosis of "paused by garbage collection" when collection takes a tenth of the workers' time or more. ([#189](https://github.com/muruhq/muru/issues/189))
- Added `muructl build`, which builds a project's environment from `uv.lock`, probes the interpreter and provides the runtime library, and made `muructl run` a build then a serve: a project with a lock and no `.venv` is built on the first run, an existing environment is used as it is and said to be behind the lock when it is, and a change to `uv.lock`, `.python-version` or `requires-python` under reload builds before the new release starts. The build mode is `when` in `[tool.muru.build]` or `muru.local.toml`, or `--build` on the command line. ([#235](https://github.com/muruhq/muru/issues/235))


## 0.1.0a12 (2026-10-31)

### Added

- When `muructl` is a container's PID 1 it starts as an init of its own, forwarding signals to the command, collecting every process that exits so none stays a zombie, and exiting with the command's status; `MURU_NO_INIT=1` leaves that to `tini` or `docker run --init`. ([#99](https://github.com/muruhq/muru/issues/99))
- Added the daemon file's `[shutdown]` table: `on_sigterm` says whether SIGTERM detaches the daemon or shuts everything down, which it does by default as a container's main process so `docker stop` drains; `delay` keeps the edge accepting while connections close after their response, and `budget` bounds the whole shutdown, which `muructl daemon stop --shutdown` now runs as the same sequence, stopping every host at once and the edge last. ([#100](https://github.com/muruhq/muru/issues/100))

### Changed

- `muructl run` reloads on change by default in development mode, as `runserver` and `flask run` do: `--no-reload` turns it off for one run and `reload = false` in the project's run table or `muru.local.toml` for good. A `[tool.muru.run]` table in pyproject.toml (`[run]` in muru.toml) holds the settings only `muructl run` has, such as the port, HTTPS and its certificate, the extra names, the reload patterns, the access log and the dashboard, and `muru.local.toml` beside the project holds the same table for one machine; a flag wins over the local file, which wins over the project's table.\n ([#85](https://github.com/muruhq/muru/issues/85))
- `[dashboard] actions` is re-read by `muructl daemon reload`, which names it in what it reloaded, and a session that has the actions is refused when it tries one while the switch is off, rather than the switch being read once at start and sessions issued earlier acting on. ([#89](https://github.com/muruhq/muru/issues/89))
- `muructl daemon reload` and SIGHUP now apply changes to the `[[listener]]` entries (a changed set takes a new edge, as an upgrade does, with no request lost), the `[limits]` table (sent to the edge in the snapshot, for connections accepted from then on) and the `[[events.sink]]` entries (started, stopped or restarted on their cursors), and the reload names what changed and what was done about it, what is unchanged and what is still read only at start. ([#158](https://github.com/muruhq/muru/issues/158))
- The edge keeps its connections to the workers a snapshot leaves unchanged and connects only to the workers new to it, so an apply no longer reconnects every worker of every application on the node, and a worker that is busy or stopped at that moment is no longer reported as unreachable and replaced by a change that did not touch it. ([#207](https://github.com/muruhq/muru/issues/207))
- `muructl daemon reload`, SIGHUP and an apply of a file that declares the host, changed or not, read every TLS host's certificate and key files again and send the edge those that changed, so a renewal hook needs one command; files that cannot be read, or do not go together, leave the host on the certificate it had rather than serving without TLS, and the reload or apply says so. ([#222](https://github.com/muruhq/muru/issues/222))
- `muructl daemon reload` and SIGHUP now apply the `[secrets.store.NAME]` tables, for the next release to start, and the `[logs]` rotation policy, and the secrets and logging guides say so; before, both were read only at daemon start and neither guide said it. ([#223](https://github.com/muruhq/muru/issues/223))

### Fixed

- Fixed one stopped or frozen worker failing every snapshot apply, so that no routing change landed for any application until the worker was replaced. A worker that does not answer the edge within its 5 s is left out of its pool and the snapshot is applied without it; the daemon records a `worker.unreachable` event naming the worker and the reason, and replaces the worker at once. ([#88](https://github.com/muruhq/muru/issues/88))
- The host is now a subreaper on Linux, as specified: a process a worker started and left behind when a hard timeout or a memory policy killed it reparents to the host, is given the shutdown timeout to finish, stopped and collected, so no zombie or stray process remains, and what the workers leave running is stopped before the host exits rather than holding its pipes open. ([#101](https://github.com/muruhq/muru/issues/101))
- Fixed `muructl apply -f FILE` leaving a release's `sendfile_roots`, and any other path it resolves from the file's directory, relative when `FILE` was given as a relative path, which the daemon then refused as not absolute. The file's directory is made absolute before its paths are resolved. ([#201](https://github.com/muruhq/muru/issues/201))
- The help of `muructl debug exec` and `muructl debug attach` says they are allowed as they are under `muructl run`, and under the daemon when its `[daemon]` table sets `debug_exec`, rather than naming the daemon's table alone. ([#202](https://github.com/muruhq/muru/issues/202))
- Fixed a worker condemned by the hard timeout and then found stalled, by the GIL stall or loop stall timeout, keeping its other requests for the hard drain grace and the shutdown timeout, which a stalled worker can never use: a stall on a worker already being replaced gives up the requests it still holds for the edge to answer and kills it a moment later. ([#203](https://github.com/muruhq/muru/issues/203))
- The daemon no longer pushes a second snapshot for the host's first announcement after a deploy, which repeated the sockets it already routed by: every deploy cost an edge apply it did not need, and a worker that was busy at that moment was left out and replaced for nothing. ([#206](https://github.com/muruhq/muru/issues/206))
- A connection accepted by the edge just before it was replaced, by `muructl daemon upgrade`, whose first request had not yet arrived, was closed without an answer. The replaced edge now waits for that request and answers it before closing the connection. ([#220](https://github.com/muruhq/muru/issues/220))

### Documentation

- The daemon guide lists, in one place, the tables `muructl daemon reload` applies and those read only at daemon start, the systemd guide links to it instead of repeating it, and the reload's message names the same tables. ([#221](https://github.com/muruhq/muru/issues/221))


## 0.1.0a11 (2026-10-30)

### Added

- `muructl run` serves the admin protocol on a socket of its own, in the
  user runtime directory, so `muructl debug` (exec and attach included,
  with no `debug_exec` needed), `metrics`, `logs --access`, `logs APP`,
  `logging`, `websockets`, `status` and `restart` work against a
  development run unchanged, and its dashboard has the workers' status,
  stacks, tasks and memory pages. A command run from the project directory
  finds the run's socket without a flag, `muructl status` says which socket
  it found, and the commands that manage releases, configuration or the
  daemon say they need the daemon. ([#68](https://github.com/muruhq/muru/issues/68))
- Added `SERVER_SOFTWARE` to the WSGI environ: `muru/` and the version, such
  as `muru/0.1.0a11`. ([#79](https://github.com/muruhq/muru/issues/79))
- An application's own static files are served by the edge in development
  and production alike: `[[tool.muru.static]]` entries in the project, or
  `--static /static/=DIR` on `muructl run` and `muructl deploy`, name a path
  prefix and a directory under the source, which the edge serves with the
  cache headers, conditional requests, ranges and precompressed variants
  of a static route, the application taking what no file answers. Under
  the daemon the release the request was routed to serves its own files,
  so a canary's pages get its own assets. ([#86](https://github.com/muruhq/muru/issues/86))

### Changed

- `muructl run` now trusts loopback peers for the forwarding headers, with
  X-Forwarded-Prefix among them, so an application behind a development
  proxy that mounts it under a path (jupyter-server-proxy, code-server) gets
  `SCRIPT_NAME` or `root_path` set with nothing to configure. Only their headers
  are trusted: a loopback peer keeps a client's connection share and
  keep-alive timeout rather than a proxy's. ([#66](https://github.com/muruhq/muru/issues/66))
- The line `muructl run` prints on macOS about reading workers from outside
  no longer reads as an error: it says stacks and profiles come from inside
  the workers, and that this is usual on macOS. ([#76](https://github.com/muruhq/muru/issues/76))
- A reload that fails under `muructl run` is reported as `release r3 failed
  to start: ...; release r2 keeps serving`, naming the attempt, which keeps
  its number as a failed deploy does under the daemon, so the fix after it
  is `r4`. ([#78](https://github.com/muruhq/muru/issues/78))
- A factory named without `factory = true` (or `--factory`) is refused when
  the application loads, rather than failing every request: a callable that
  takes no required arguments cannot be a WSGI or ASGI application, and the
  message names the setting. The same check refuses a callable that does not
  take the arguments its interface passes, two for WSGI and three for ASGI,
  naming the interface to declare when it fits the other one. ([#80](https://github.com/muruhq/muru/issues/80))
- `muructl run --reload-include` adds patterns to the default `**/*.py`
  rather than replacing it, so `--reload-include '**/*.html'` alone still
  reloads on a changed module; an exclude wins over an include, so
  `--reload-exclude '**/*.py'` watches the included patterns alone. The
  start-up line names the directories and patterns watched. ([#83](https://github.com/muruhq/muru/issues/83))
- A deploy carries every setting a flag can set forward from the previous
  release when neither its flags nor the source's `[tool.muru]` table give
  it: timeouts, the memory policy, environment variables and secrets by
  name, hooks, preload and the rest, as the entrypoint, interpreter,
  processes and threads already were, so a `--timeout soft=3s` given once
  no longer vanishes on the next plain `muructl deploy`. Tables come from
  the source alone, and `muructl deploy --fresh` ignores the previous
  release. ([#84](https://github.com/muruhq/muru/issues/84))
- A request that condemns its worker is answered at once rather than after
  the drain grace and the shutdown timeout: the hard timeout answers the
  stuck request `504 hard_timeout` when it replaces the worker, and a loop
  or GIL stall, or a worker gone silent, answers every request the worker
  held `502 worker_stalled`, while the worker's other requests still get
  `hard_drain_grace` to finish. A worker is silent once its next report is
  the GIL stall timeout late, rather than the moment a report is late, and
  a worker condemned by the silence check and its own stall event is
  replaced once. ([#87](https://github.com/muruhq/muru/issues/87))

### Fixed

- Fixed relative paths in an applied file being taken from the daemon's
  working directory rather than the file's: a route's or an overlay's
  `static` directory, `sendfile_roots`, a host's and a route's error page
  files, an upstream's `tls.ca` and a client class's lists are now
  resolved from the file's directory, as `source` and a host's TLS files
  already were. `muructl run` reads `--tls-cert`, `--tls-key`, `--python`
  and `--reload-dir` relative to the project (`--chdir` or the directory
  given) rather than the current directory, and `--print-config` writes an
  interpreter inside the project, such as its `.venv`, as a relative path.
  A host's or route's error page file that does not exist is refused by the
  apply, rather than by the edge once the snapshot reaches it. ([#64](https://github.com/muruhq/muru/issues/64))
- Fixed a worker's `print()` output being lost. The worker's standard output
  is now line-buffered, so each line reaches the terminal or the release log
  as it is written, and whatever is still buffered is flushed when the worker
  exits. ([#65](https://github.com/muruhq/muru/issues/65))
- Fixed the hard timeout not firing for a request stuck in C code holding
  the GIL once a second request reached the same worker: the second
  request's arrival blocked a link thread on the interpreter, which stalled
  the worker's reports to the host. A request's lookup of its mount no
  longer touches the interpreter, and the reports come from a thread of
  their own. ([#67](https://github.com/muruhq/muru/issues/67))
- `breakpoint()` under `muructl run` names a command that works: the line
  reads `muructl debug attach APP --worker N` with the application as the
  run calls it, and the attach opens a pdb session in the request through
  the run's admin socket, with no `debug_exec` to set. ([#69](https://github.com/muruhq/muru/issues/69))
- Fixed a held release of the default application, with no host declared,
  not answering a `Muru-Preview` header or cookie: the request went to the
  serving release. The `release.ready` event also says "1 worker" rather
  than "1 workers". ([#70](https://github.com/muruhq/muru/issues/70))
- Fixed a WebSocket that had sent nothing since it connected being killed
  with 1011 when its worker stopped, after `shutdown_timeout`, rather than
  closed with 1001 at once as a socket that had exchanged a message was. ([#71](https://github.com/muruhq/muru/issues/71))
- The development dashboard's In flight tile counts requests not yet
  answered, as `muru_requests_in_flight` does, rather than staying at 0
  while WebSockets are open, and the workers table's column is now Busy:
  slots handling a request or holding an open WebSocket. ([#75](https://github.com/muruhq/muru/issues/75))
- WSGI application threads carry the runtime's names, `muru-app-0` and so
  on, in Python too, so logs and thread listings no longer call them
  `Dummy-1`. ([#77](https://github.com/muruhq/muru/issues/77))
- `muru.context().release` under `muructl run` is the release the terminal
  names, `r1` and then `r2` after a reload, rather than `dev` every time. ([#81](https://github.com/muruhq/muru/issues/81))
- Fixed a run directory too deep for the workers' socket paths, or given as
  a relative path, failing only once a release started, or after a 30 s wait
  for a daemon that never answered. `muructl daemon start` and the daemon
  refuse it at start, saying how much room an application's name has, and a
  release's host says so too when its socket directory is too long. ([#82](https://github.com/muruhq/muru/issues/82))
- Fixed a profile of workers read from outside, on Linux, dropping a worker
  after one failed read, which could leave it with no samples while requests
  were starting and ending: a failed read is now skipped and the worker
  sampled again, and a stacks read is tried again. Such a profile also names
  each function by the line it starts on, as a profile read from inside
  does, rather than by the line that was running, so a function's samples
  are counted together.
- Fixed `muructl debug attach` detaching with the session reported as still
  open when pdb wrote once more before the debugged code went on: the
  detach now waits for the session to end.

### Documentation

- Added eight tutorials: an ASGI application with WebSockets, deploying with the daemon and rolling back, a first canary, moving from gunicorn to Muru, a first Django site, scaling under load, finding a slow request, and two sites on one node. The Flask and Django guide now says how to serve Django's static files under `muructl run`, which `DEBUG` alone does not do.
- Corrected the status notes of the mounts, secrets, metrics and worker recycling guides, which still listed as coming the groups `mounts` subset, per-application users, the worker CPU and scaling metrics and per-group memory and timeout settings.


## 0.1.0a10 (2026-10-28)

### Added

- Added `concurrency` and `max_connections` on an ASGI mount: the requests
  it runs at once on each worker, as slots of its own at the edge, and the
  open WebSockets it holds apart from them, so sockets on a `/ws` mount no
  longer take the WSGI application's threads.
- Added `host` and `method` lists to a worker group's `match` table: a
  group takes a request when its path is under one of the group's paths
  and every list given names the request's client class, host or method.
  A group asking more of a request comes before one asking less, and two
  groups asking the same of the same path are refused.
- Added `muructl restart APP[@RELEASE] --worker N`, which replaces one worker
  with a fresh one the way a recycle does, with `--pid` to name it by process
  id, `--all` for every worker in turn, `--force` to take it out of routing
  and kill it at once with its stacks in the log, and `--no-wait`. The event
  is `worker.restarted`. A restart resets the worker's state but loads no new
  code; new code is a new release.
- Added a worker group's `mounts` list, the named mounts its workers serve
  beside the default mount: the host still imports every mount, a worker of
  the group realises and declares only its own, and a group's paths must lie
  within the mounts it serves.
- Added optional worker groups: a group with `optional = true` is one the
  release is ready without. Its workers join as each listens, its requests
  wait for one until the queue timeout rather than going to the default
  group, and a worker of it that cannot start is given up instead of
  stopping the release.
- Added per-group timeouts and memory policy: a worker group's `timeouts`
  table may set the soft, hard, hard drain grace, loop stall and GIL stall
  timeouts, and its `memory` table any memory limit, each key the release's
  when absent. `shutdown_timeout` and `websocket_drain_grace` stay with the
  release.
- Added the GIL stall timeout: every worker, WSGI or ASGI, now has a native
  thread that attaches to the interpreter every half second, and a worker in
  which no thread has run Python for `gil_stall_timeout` (30 s by default,
  `--timeout gil_stall=`) is replaced as for the hard timeout, with a
  `worker.gil_stalled` event and its stacks in the log. The host also
  replaces a worker whose reports have stopped for that long, such as one
  stopped by a signal.
- Added the dashboard actions beside maintenance mode: a session that may
  act can promote a ready release, roll an application back, drain a
  release, pin a worker group's count or unpin it, and restart a worker,
  each a form on the page or a POST to `/actions/NAME` for a script, and
  each recorded as a `dashboard.action` event.
- Added the edge's retry of a request no worker has read: when the worker
  chosen cannot be connected to, or closes its connection before the
  request head reaches it, as a worker that is stopping does, the request
  goes to another worker. Retries are counted in `muru_request_retries_total`
  and marked `retried` in the access log; a second failure is 502
  `worker_unavailable`. A request a worker has read is never sent again.
- Added worker inspection to the daemon's dashboard: each running release
  links to a page of its workers, with their status refreshed every two
  seconds and their stacks merged across workers, and each worker to a page
  of its own with its status, stacks, asyncio tasks and, on request, its
  memory, as `muructl debug` shows them; the same answers are served as JSON
  at `/v1/debug/OP?release=APP@RELEASE`.

### Fixed

- Fixed Ctrl-C on `muructl run` answering requests in the stopping window
  with 502 `worker_crashed`, and the host logging its stop twice: the edge
  now stops accepting and finishes the requests it holds before the host is
  told to stop, and only `muructl` tells it, since the host and its workers
  no longer share the terminal's process group.


## 0.1.0a9 (2026-10-27)

### Added

- Added `muructl release show`, which prints one release's state, source,
  interpreter, entrypoint and sizes, with its worker groups and mounts, and the
  interface each entrypoint was detected as when it is declared `auto`.
- Open WebSockets are now visible: the `muru_websocket_connections` gauge,
  `muru_websockets_closed_total` by who closed and how, and the
  `muru_websocket_duration_seconds` histogram; `muructl websockets`, which
  counts open sockets by endpoint with identifiers in paths folded together,
  or lists them with `--list`; the count in `muructl status` and
  `muructl release show`; and the dashboards' WebSockets table, tile, chart
  and `/v1/websockets`.

### Changed

- A WebSocket's request now ends when the application accepts it: its
  access log line is written then, with the handshake's time, and a second
  line with the same request id is written when it closes. Its lifetime no
  longer goes into `muru_request_duration_seconds`, the latency charts or
  the rollout gates, and it no longer counts in `muru_requests_in_flight`
  once accepted.
- `muructl run` now prints a line for each worker group and each mount when it
  starts, the host says what each mount's entrypoint is served as (also when
  the workers import the application themselves), the serving line names the
  event loop a WSGI release has beside its threads when a mount is ASGI, and
  counts of one are singular: "1 worker of 4 threads".

### Fixed

- A worker told to stop no longer leaves its open WebSockets to be cut off:
  after the new `websocket_drain_grace` timeout (none by default) it closes
  each with 1001, which the application sees as `websocket.disconnect` and
  the client as a close it can reconnect after, where before the client got
  1011 once the worker was killed or gave up 30 seconds later. Workers are
  now killed `websocket_drain_grace` plus `shutdown_timeout` after being
  told to stop, as the timeouts guide says, and a drained release's workers
  no longer outlive the release being reported stopped.

### Documentation

- The Flask and Django guide no longer says a Flask client sees Muru's error
  page for an exception: Flask answers with its own page unless
  `PROPAGATE_EXCEPTIONS` is set, and Django unless `DEBUG_PROPAGATE_EXCEPTIONS`
  is set, as the guide now shows.


## 0.1.0a8 (2026-10-27)

### Changed

- Both dashboards may now be shown in a frame by a page of the same origin,
  such as a JupyterLab pane through jupyter-server-proxy; other sites still
  may not frame them.

### Documentation

- The README and the documentation now describe what the daemon does today,
  with canaries, traffic policies, scaling and the other features added in
  0.1.0a5 and 0.1.0a6, instead of describing development mode as the only
  part that works.


## 0.1.0a7 (2026-10-27)

### Added

- Added `muructl dashboard login --token`, a bearer token for scripts that read the dashboard's JSON gateway or act through it, and `--url` (or `MURU_DASHBOARD_URL`) to print the login link under the address a tunnel or proxy serves the dashboard at. The dashboards' pages now use relative addresses, so they work behind a proxy that serves them under a path, such as jupyter-server-proxy.

### Changed

- CLI help, messages and warnings no longer refer to the sections of the design specification or to implementation phases, and the `muructl run` warnings that said the daemon was still to come now point to `muructl daemon start`.
- The daemon's dashboard and JSON gateway now need a signed-in session for every page, not only for the actions: `muructl dashboard login` gives a link for a session that may look, `--actions` one that may act, which `actions = true` in the `[dashboard]` table must still allow, and `/metrics` needs none. Scripts that read the gateway need a token from `muructl dashboard login --token`.


## 0.1.0a6 (2026-10-27)

### Added

- Added CORS for gRPC-Web: a release's `grpc_web_origins` has the edge answer preflights and add the CORS headers to calls from the origins it names.
- Added TLS fingerprints: the edge computes the JA4 of each TLS connection's ClientHello, client class rules match it with `ja4` and `ja4_list`, and applications find it in `muru.tls`.
- Added `h2c` on a listener, naming the clients that may speak HTTP/2 in cleartext on it; another client opening with HTTP/2 is answered 505, and its HTTP/1.1 requests are served.
- Added `muru.deadline` to the WSGI environ and `deadline` to every ASGI scope's `extensions["muru"]`: when the edge gives up on the request, in seconds since the epoch.
- Added `muructl config export --resolve-rollouts`, which writes a rollout under way as where it stands: the candidate declared beside the serving release, and the application's routes splitting traffic between them at the candidate's current share.
- Added `muructl debug attach --dap`, which carries the Debug Adapter Protocol between an IDE and debugpy in a worker, through muructl, a waiting `breakpoint()` stopping at its next line once the IDE has attached.
- Added `muructl debug memory --snapshot`, `--diff` and `--drop`, to compare a worker's allocations between two chosen moments, and a traced worker taking no new requests while another worker of its group can.
- Added `muructl debug profile --path` and `--requests` to sample only the requests chosen, and `--format pstats` to write the samples as Python's pstats.
- Added `muructl debug profile`, which samples every thread running Python in a release's workers for a period and writes collapsed stacks for a flame graph, `debug trace`, which counts calls and time with sys.monitoring (Python 3.12 and later), `debug memory`, which reports sizes, gc state and the commonest objects and traces allocations with tracemalloc over a period, and `debug exec`, which runs a snippet in the workers when the daemon's `[daemon] debug_exec` allows it. A request past the soft timeout is now the event `request.soft_timeout` with where it was, and the `worker.timeout_killed` and memory `worker.recycled` events say where the stuck request was and what the worker held most of.
- Added client class routes to overlays: `routes.NAME.client_routes` gives a client class a route of its own at the route's host and path while the overlay is active, changing the route for that class alone or adding a copy of the route, and `muructl overlay preview`, `promote` and `config show --effective` cover them.
- Added client classes and rate limits in applied files: `[[client_class]]` and `[rate_limit.NAME]` tables that `muructl apply` reads are recorded and reach the edge, after the daemon file's, and `muructl config export` writes them out.
- Added client classes and rate limits: `[[client_class]]` rules in the daemon's configuration file classify each request by user agent (substrings, regular expressions or a list file read again when it changes), address, headers and cookies, with claimed crawlers confirmed by DNS, and the class reaches the access log, the `client_class` label of `muru_requests_total` and the application as `muru.client_class`; `[rate_limit.NAME]` token buckets, keyed by address prefix, class, header or cookie and named by hosts and routes, reject with 429 and Retry-After, delay or only log, counted in `muru_rate_limited_total`; and a route's `methods` refuses methods with 503 `read_only` or 405, while its `mode` reaches the application as `muru.mode`.
- Added debugger attach: `breakpoint()` in a worker waits for `muructl debug attach`, out of the request timeouts, and `debug attach` opens a pdb session there, carried line by line through the daemon, or on a thread of its own when no breakpoint waits; it is allowed where the daemon's `[daemon] debug_exec` allows exec.
- Added draining and pinning of a debugged worker: while a `muructl debug attach` session is open the worker takes no new requests and is not recycled, killed or scaled in, and `muructl debug status` shows it as `debugging`.
- Added error page selections to overlays: `routes.NAME.error_pages` chooses the route's page for a status or `status:reason` while the overlay is active, each key in place of the route's own, and `muructl overlay preview` and `config show --effective` show the pages chosen.
- Added gRPC mounts: a mount or a release with `interface = 'grpc'` serves the services a registration function adds with grpcio-generated code, synchronous handlers on threads and asynchronous ones on the event loop, all four kinds of call over h2 and h2c, each call balanced across the workers, with grpc-timeout deadlines, request decompression, the standard health service answered by the edge, server reflection with `reflection = true`, and gRPC-Web translated at the edge. grpcio has no free-threaded wheels, so gRPC mounts need a regular build.
- Added overlays: `[overlay.NAME]` tables in applied files change named routes (site mode, method restrictions, a traffic pin or split, the target and rate limits) and applications' maintenance mode while active, without changing the base configuration, activated by hand with `muructl overlay activate` (for a time or until one) or by a one-off or recurring (cron and duration) schedule in a named time zone, advisory schedules that only report, latched ones that stay active until ended, warm-up that starts a release ahead of each window, with preconditions on a release being ready, conflicts between overlays refused unless priorities settle them, and `muructl overlay list`, `end`, `preview` and `promote`. Routes take a `name` by which overlays reach them. A signed webhook, served from the daemon's `[overlay_webhook]` table, activates and ends overlays declared with `trigger.external = true`; `muructl config show --effective` shows the configuration with the active overlays applied and what each sets; and the daemon warns when the system clock that schedules follow is not synchronised.
- Added reading workers from outside with PEP 768 on Linux and Python 3.14 or later: `muructl debug stacks`, the stacks captured before a hard-timeout kill, and `muructl debug profile` read the worker without its cooperation, so a worker holding the interpreter still shows its stacks.
- Added rollouts for an application reached as the default on a daemon with no host declared: the canary's share and previews apply to its requests as a route's policy would.
- Added rollouts, scaling and maintenance mode to the daemon's dashboard: the rollouts under way and the latest that ended, each worker group's workers, policy, CPU, GIL wait, loop lag, diagnosis and last decision, and when an application went into maintenance, on the page and as `/v1/rollouts` and `/v1/scaling`.
- Added routing on a client class: a route's `match = { client_class = [...] }` makes it a route for those classes, so routes for different classes may share a path; a traffic policy's `classes` gives a class a release, or an upstream, of its own; and a worker group's `match` takes those classes' requests ahead of the other groups. `muructl apply` refuses classes the daemon's file does not define.
- Added scale to zero: a spare or target group with `min_workers = 0` drains its last worker once idle and the next request waits at the edge while one forks, `host_idle_timeout` stops an idle release's host too until the next request starts it, and a retained release keeps one worker per group (`muructl deploy --retain-workers`), dropping to its host alone and then stopping when the node is over its memory budget.
- Added scheduled capacity and deploy freezes: `[[app.NAME.schedule]]` entries, one-off or recurring, raise a worker group's floor and ceiling for their windows, with warm-up, `scale_in = 'hold'` and the floor shared by traffic during canaries; overlays set the same under `apps.NAME`; `muructl scale --min N --until TIME` sets a floor for a while; and `freeze_deploys` refuses deploys, applies that deploy, rollouts and promotions unless `--override-freeze` is given, which is recorded as an event.
- Added shared rate limit buckets: under the daemon the buckets are in shared memory, so a replaced or restarted edge draws from the buckets its predecessor left.
- Added the application-level site mode: an overlay's `apps.NAME.mode`, with `deny` and `answer`, sets the mode and refused methods on every route of the application while the overlay is active.
- Added the dashboard's first action: with `actions = true` in the `[dashboard]` table, a browser signed in through a single-use link from `muructl dashboard login` turns an application's maintenance mode on and off from the page, each action CSRF-protected and recorded as a `dashboard.action` event.
- Added the loop stall timeout: an ASGI worker whose event loop has not run for `loop_stall_timeout` (10 s by default, `--timeout loop_stall=`) is replaced as for the hard timeout, with the event `worker.loop_stalled`.
- Added the node's budget: the `[daemon.budget]` table caps the workers, memory and CPU the releases on the node hold between them, scale-outs are admitted against it with the highest `[app.NAME] priority` first, a release whose memory does not fit is not started once retained releases have made way, schedules are checked against it when applied and ahead of each window, and `muructl daemon status` shows what is held.
- Added the release settings' layers to `muructl config show --effective`: each serving release's entrypoint, interpreter, sizes, recycling limits, timeouts, memory policy and redirect settings, with whether each came from the release table or deploy flags, `[tool.muru]`, the source's `.venv`, the previous release or the default.
- Added the workers' own scaling signals: CPU time, GIL wait and event loop lag, reported every memory check interval, shown by `muructl scale status` with a diagnosis of each group's bottleneck and the tuning it suggests, which every `scaling.decision` event carries too, and `muru_worker_cpu_seconds_total` in the metrics.
- Added threads of a mount's own: a WSGI mount's `threads` is a pool only it uses, or `{ reserved, max }` to reserve some and borrow the shared threads beyond them, and a gRPC mount's `threads` sizes its handlers' pool.
- Added two canary gates: `--max-p99` on the candidate's 99th percentile request time, and `--max-worker-faults` (0 by default) on its worker crashes and timeout kills, which rolls back at once, before the step has the requests the other gates need.
- Added two canary gates: `--max-queue-wait`, the candidate's 95th percentile wait for a worker thread, and `--probe URL`, repeatable, a URL requested through the edge from the candidate and the baseline every 5 seconds by preview, which rolls back when it fails on the candidate alone.
- Added upload offloading: a route with `upload = 'multipart'` or `'raw'` makes the edge receive the body and store each file before the application sees the request, hashing it as it arrives, and pass the files to the worker by descriptor; a multipart form arrives with each file field replaced by `FIELD.name`, `content_type`, `size`, `sha256` and `ref` fields, a raw upload with an empty body, and `muru.uploads.files()` and `get()` give upload objects with `open()`, `save()`, which links the file into place where it can, and `path`. A route's `max_body_size`, `max_file_size`, `max_files`, `hash` and `spool_quota`, and the daemon's `[limits] spool_quota`, are answered at the edge with 413 `upload_too_large` or `too_many_files` and 507 `upload_spool_full`, and `precheck = true` asks the application with the headers alone before the body is read.

### Changed

- A static worker group left above its count when a pin is lifted now returns to its count one worker a minute, instead of keeping the pinned count until the next deploy.

### Fixed

- A release is now `ready` only once the edge has been given it, so a request made as soon as `muructl release wait` returns reaches the new release; before, the wait could return a moment before the edge routed to it, and the request was answered 503.
- Fixed a client class rule whose `user_agent_list` or `address_list` held no entries matching every request: an input from an empty list now matches nothing.
- Fixed a native deploy after a managed server release failing to start: it took the server release's worker count of zero as its own.
- Fixed a worker whose default mount is WSGI running on a single thread when it also had an ASGI or gRPC mount: it now has its `threads`.
- `Upload.save()` on Linux with Python 3.13 and earlier now links an upload without a spool path into place, as it does on 3.14, instead of copying it.
- Fixed the daemon freezing when the routing changed while a worker was stopped or frozen: the edge now gives up on a worker that has not answered HELLO within 5 s and keeps its routing, the change fails naming the worker, and the daemon waits at most 30 s for an edge to apply a snapshot.

### Security

- Updated the zstd library the edge and the daemon use to 0.14.3, for RUSTSEC-2026-0391, RUSTSEC-2026-0398 and RUSTSEC-2026-0403.

### Documentation

- Documented A/B cohorts by header or cookie, a client class given a release by a traffic policy's `classes`, and routes of one application on different releases, in the traffic guide.
- The PROXY protocol is no longer listed as coming: it moved to the specification's future ideas (ADR 0029), and the trusted proxy settings remain the way a proxy passes on the client's address.


## 0.1.0a5 (2026-10-25)

### Added

- Added HTTP upstreams: an `[upstream.NAME]` table names an unmanaged HTTP server, by `host:port` or `unix:/path` addresses and `http1`, `h2c` or `h2` with TLS, that a route, a static route's `fallback`, a traffic share named `upstream:NAME` or an authentication sub-request can target; requests are re-serialised with forwarding headers and re-framed bodies, addresses take requests in turn, one whose requests fail is ejected and tried again gradually, a `health` table probes a path, `muru_upstream_health` reports each address, `trust_muru_headers` lets an upstream redirect internally, and `muructl upstream list` shows what is applied.
- Added authentication sub-requests: a route's `auth` table names an application path the edge asks, with a bodiless GET carrying the client's headers, before it serves the route, whether an application, a static directory or a WebSocket upgrade; 2xx allows the request and adds the answer's `copy_headers` to it as trusted headers, 401, 403 and redirects go back to the client as the handler answered them, and anything else is 503 `auth_unavailable`; the handler sees `muru.auth_request`, `muru.original_method`, `muru.original_uri` and `muru.original_host`, allowing decisions can be cached with `cache = { ttl, key }`, and `muru_auth_requests_total` counts decisions by result.
- Added custom error pages and maintenance mode: `error_pages` by status or `status:reason` on a route, a release (or `[tool.muru]`), a host or in the daemon's file, each a `file`, a `template` with `{status}`, `{title}`, `{reason}`, `{description}`, `{request_id}`, `{retry_after}`, `{host}` and `{path}`, or an application `path` the edge asks for the page as an internal sub-request that sees `REDIRECT_STATUS` and `muru.error_reason`; `error_override` to replace an application's own error responses (`when` = `all`, `empty_body`, `not_json` or `html_only`, opt out per response with `X-Muru-Error-Override: off`); and `muructl maintenance on|off APP`, which serves 503 `maintenance` at the edge with exemptions by path, address and bypass cookie from the release's `[maintenance]` table.
- Added file hints: a `wsgi.file_wrapper` object returned as the response iterable, and the ASGI `http.response.pathsend` and `http.response.zerocopysend` extensions, pass a duplicate of the file's descriptor to the edge on a FILE frame, which sends the range itself while the application's file is closed; an `X-Muru-Sendfile` response header, or the second header `sendfile_header` names such as `X-Sendfile`, makes the edge serve a file inside the release's, mount's or trusted upstream's `sendfile_roots` with static-file behaviour, and `muructl deploy --sendfile-root DIR` and `--sendfile-header NAME` set the keys.
- Added internal redirects: an application answers with `X-Muru-Redirect: /path` (or `X-Accel-Redirect` and the CGI local redirect where a release or mount opts in) and the edge serves the request from there, a protected file from a static route marked `internal = true` or another application path, with the client's headers kept, the application's `Content-Disposition` and cache headers carried over, `REDIRECT_URL`, `REDIRECT_QUERY_STRING` and `REDIRECT_STATUS` for the target, and `muru.is_internal()` to tell such a request. Routes and mounts can be `internal` and name the applications whose redirects may reach them in `accept_redirects_from`; a client's own `X-Muru-*` request headers never reach an application.
- Added managed HTTP server releases: a release with `kind = 'http'` runs a `command` such as gunicorn, uvicorn, Granian or mod_wsgi-express (or any server with the `generic` profile), as instances the daemon binds to Unix sockets or loopback ports, probes for readiness, restarts when they exit and stops with the profile's signal; the edge routes to it as an upstream named after the release, so traffic policies, canaries, drains and rollbacks work between it and native workers of the same application, and `muructl deploy --server`, `apply`, `config export` and `release list` know it.
- Added rollouts: `muructl deploy --hold` starts a release without traffic, `muructl rollout canary` gives it a growing share of the application's requests step by step with gates on its 5xx rate and p95 judged against the release that serves, rolling back on a failing gate and promoting when every step passes, `muructl rollout bluegreen` moves all traffic at once, and `rollout status`, `pause`, `resume`, `promote` and `abort` follow and control a rollout. Each change is a `rollout.*` event.
- Added the autoscaler: a `scaling` table on a release or a worker group with the spare and target modes, bounds, windows and a churn budget sizes the group from the edge's slot and queue signals, every decision an event with its reasons; `muructl scale status` shows the policy, workers, signals and churn, `muructl scale --processes N` pins a group's count and `--unpin` hands it back, and `muru_workers_desired`, `muru_scaling_decisions_total` and `muru_churn_reaps_total` join the metrics.
- Added the soft timeout: `soft_timeout` in a release's `timeouts` table, or `--timeout soft=30s`, interrupts a request that runs past it by raising `muru.RequestTimeout` in its thread (waking one that waits for the request body) or cancelling its ASGI task, logs where it was, and answers 504 `soft_timeout` unless the application catches the exception and answers for itself. The hard timeout must exceed the soft timeout by at least 5 seconds.
- Added traffic policies: a route's `traffic` table shares requests between an application's releases by percentage, with clients hashed to stable buckets by an anonymous id cookie the edge issues, a header, a cookie or their address; `sticky = true` keeps a client on its release through a cookie, with a grace period before a superseded release drains; and the `Muru-Preview` header or `muru_preview` cookie pins a release, including one with no share. Releases a policy names are kept running. `muru.context().release` now reports the release id.

### Fixed

- A client whose WebSocket message was over the limit, or whose frame was malformed, could see the connection reset instead of the 1009 or 1002 close frame: the edge now reads away what the client is still sending before it drops the socket.
- Streaming responses (`text/event-stream` or `X-Accel-Buffering: no`) and accepted WebSockets no longer count toward the hard timeout, which replaced the worker of a long-lived WebSocket as if its request were stuck.
- The daemon's once-a-second work (host and edge checks, retention, log rotation, rollout gates and the autoscaler) no longer stalls while admin requests arrive more often than once a second.
- `muructl daemon stop` and `muructl daemon upgrade` bound every exchange with the daemon, so a daemon that accepts a connection and never answers cannot hold the command past its `--timeout`; the daemon also closes its API connections once the shutdown grace period ends.


## 0.1.0a4 (2026-10-25)

### Added

- Added `muructl daemon upgrade`, which re-executes the daemon from the
  installed binary without interrupting traffic: the daemon keeps its pid,
  takes its hosts and edge over again, and replaces the edge with one from
  the new version, the old edge finishing its connections first.
- Added `muructl debug status`, `stacks`, `tasks` and `inspect`, which ask a
  release's workers what they are doing through the daemon and the host,
  and `muructl logging level`, which changes logger levels in the workers
  for a period.
- Added built-in log rotation: the daemon rotates the access log, the
  edge's and each release's log and its own daily or at a size, compresses
  the archives and keeps the newest, from the `[logs]` table; `rotate =
  'external'` leaves rotation to logrotate.
- Added charts over time to the dashboards (requests, errors and latency
  over the last hour or day), served as JSON on `/v1/history` and streamed
  with Server-Sent Events, and the daemon's events stream on
  `/v1/events/stream`, which the page uses to show events as they happen.
- Added connection limits: `max_connections` per listener with a fast 503
  when full, `max_connections_per_client`, keep-alive timeouts (longer for
  trusted proxies), `max_requests_per_connection` and the HTTP/2 stream
  limit, in the daemon's `[limits]` table or as `muructl run` flags, with
  the open and refused connections on `/metrics`.
- Added memory limits for workers: `[tool.muru.memory]` or `--memory
  KEY=VALUE` set a soft limit, over which a worker reclaims and is then
  recycled, and a hard limit, over which it is replaced at once, measured
  as USS, PSS or RSS; each worker's memory and requests are served on
  `/metrics`.
- Added per-application users: `[app.NAME]` takes `user`, `group` and
  `umask`, which a daemon running as root applies to the application's
  hosts and workers, and a daemon that is not root reports as not applied.
- Added response compression at the edge: responses the client accepts are
  compressed with br, zstd or gzip, for the configured content types and
  sizes, from the daemon's `[compression]` table or by default under
  `muructl run` (off with `--no-compression`).
- Added secrets: stores of the file, env and exec kinds in the daemon's
  configuration, secret references in a release's env table or with
  `deploy --secret`, delivered as variables or files when the release
  starts and never recorded or shown, and `muructl secret` for file stores.
- Added static routes, which serve a directory from the edge with index
  files, conditional requests, ranges, precompressed variants, cache
  headers and a fallback to an application or a file, and `muructl
  serve-static DIR` for serving a directory in development.
- Added worker recycling and the hard timeout: `max_requests` and
  `max_lifetime` replace a worker with a fresh one on a new socket with no
  request failed, and a request past `--timeout hard=DURATION` has its
  worker taken out of routing, its stacks written to the log and the worker
  replaced, with `worker.recycled` and `worker.timeout_killed` events.

### Changed

- Changed a daemon that runs as root to start the edge, and every
  application that names no `user`, as a service user (`muru`, or the one
  `--user` or the `[daemon]` table names) and never as root; it refuses to
  start when the account does not exist, and `user = 'root'` on an
  application is recorded as an `app.user_is_root` warning each time a
  release starts.

### Fixed

- Fixed `muructl run` and `muructl serve-static` being killed by a Ctrl-C or
  SIGTERM that arrived while the workers were still starting; the run now stops
  in order, the workers running their drain and exit hooks, whenever the signal
  comes.
- Fixed a relative `--python` for `muructl deploy`, or a relative `python`
  in a file for `muructl apply`, being resolved where the daemon starts
  the host rather than where the command ran or the file is; a bare
  command name is still looked up on the PATH.
- Fixed the dashboard's history and events streams delivering only their
  first message after the page loaded, so the charts and the events table
  stood still until a reload.


## 0.1.0a3 (2026-10-25)

### Added

- Event webhooks: `[[events.sink]]` entries in the daemon's configuration file deliver events as signed CloudEvents requests, filtered by type patterns and levels, in order and at least once from a cursor that survives receiver outages and daemon restarts.
- Logs under the daemon: `muructl logs APP[@RELEASE]` shows or follows a release's output and `muructl logs --access` the access log, whose records now name the application, release, host, group, mount and snapshot; `muructl logs reopen [--workers]` (or SIGUSR1 to the daemon) reopens every log file after logrotate has renamed them.
- Mounts: `[mounts.NAME]` tables in `[tool.muru]` or under a declared release serve other entrypoints at path prefixes from the same workers, any mix of WSGI and ASGI, with the mount's path as `SCRIPT_NAME` or `root_path`.
- Prometheus metrics: the daemon serves request counters, timing histograms, in-flight, queue depth and slot gauges and the workers ready at `/metrics` on the address `--metrics` or the `[metrics]` table names, `muructl metrics` prints the same text, and a development run serves it from the dashboard.
- Python logging defaults: a `[tool.muru.logging.python]` table (or a declared release's `logging.python`) sets the root logger's level, the format of loggers without handlers (`standard`, `json` or a format string), warning capture and named loggers' levels before the application is imported, published as `MURU_LOG_LEVEL` and `MURU_LOG_FORMAT`, with `muru.logging.default_config()` giving the same defaults as a `dictConfig` dictionary.
- Running under systemd: the shipped unit starts the daemon as Type=notify and the daemon reports readiness with sd_notify; SIGTERM detaches it as `muructl daemon stop` does, `muructl daemon reload` (or SIGHUP) re-reads the `[proxy]` table, and `muructl daemon stop --timeout` bounds the wait for the daemon to exit.
- The daemon serves a read-only dashboard and JSON gateway on a loopback address given with `--dashboard` or a `[dashboard]` table: the node, every application's releases with their workers and slots, hosts and routes, recent requests, events and each serving release's log tail, with `/metrics` beside them.
- The daemon: `muructl daemon start` (or `muructl up`) runs a daemon with
  its own state store, runtime and log directories, and `muructl deploy APP
  SOURCE` creates a release from a source directory, starts it, moves
  traffic to it when every worker is ready and drains the release that
  served before, so no request is refused during a deploy; the replaced
  release stays warm for 30 minutes (`--retain`) so a rollback within that
  window is instant. `muructl release
  list` and `wait`, `rollback`, `drain`, `events` and `status` manage and
  watch it; a release whose host dies is started again. The edge runs as a
  process of its own on a socket the daemon binds and hands to it, and is
  started again on the same socket if it dies. `daemon stop` detaches,
  leaving the edge and the hosts serving for the next daemon to take over,
  so a daemon restart costs no request; `daemon stop --shutdown` drains
  and stops everything.
- Trusted proxies: a `[proxy]` table in the daemon's configuration file names the peers whose Forwarded, X-Forwarded-For, -Proto, -Host, -Port, -Prefix and X-Real-IP headers are believed; forwarding headers from anyone else are removed, and the resolved client, scheme, host, port and prefix reach the application, with the proxy in `muru.proxy_addr`.
- Virtual hosts and TLS in the daemon: `[host.NAME]` and `[[route]]` tables applied with `muructl apply`, listeners with `tls = true` serving each host's certificate by SNI, a default host and 421 for unknown names, path-prefix routes, HTTPS redirects, `muructl host list` and `muructl route list`, and SERVER_NAME from the matched name.
- Worker groups: `[groups.NAME]` tables in `[tool.muru]` or under a declared release give a release named groups with their own process and thread counts, selected by path prefix at the edge, with the group in `muru.context()` and `MURU_GROUP`.
- `muructl apply -f FILE or DIR` deploys the applications and releases a declarative file declares, with `--dry-run`, `--prune` and `--json`; `muructl config export` writes desired state as such a file and `muructl config diff -f` compares one with it. Release tables take a `source` key, which `muructl run --print-config` now writes.
- `muructl run` now serves ASGI applications: an entrypoint whose callable is
  a coroutine function, or one declared with `--interface asgi`, runs on one
  event loop per worker with the lifespan protocol (startup before the worker
  is ready, shutdown on stop, and the lifespan state copied into every
  request's scope), request bodies as `http.request` messages,
  `http.disconnect` when the client goes away or the response completes, a
  sync thread pool for `asyncio.to_thread` and `sync_to_async`
  (`--sync-threads`, default 8) and `--concurrency` (default 1000) requests
  in flight per worker. WebSockets, trailers and the file extensions follow.
- `muructl run` serves WebSockets to ASGI applications: the edge completes
  the handshake once the application accepts (with its subprotocol and
  headers), relays whole text and binary messages in both directions, carries
  close codes each way, closes a socket with 1009 for a message over 1 MiB,
  pings idle clients and closes a silent socket after five minutes. Closing
  before accepting answers 403, and the `websocket.http.response` extension
  lets the application decline with a full response. HTTP/2 extended CONNECT
  is accepted as well.

### Fixed

- An application that fails after starting its response but before any of it
  has reached the edge now gets the 500 error page, with the traceback in
  development mode, instead of a dropped connection.
- The edge binds its HTTPS listener together with the HTTP one, before it connects to the workers, so a client that finds one port open during start-up finds both.
- `muructl run` now stops in order on SIGTERM as it does on Ctrl-C, and a host
  whose parent has gone away still stops its workers; before, both could
  leave worker processes running.

### Documentation

- Starlette, FastAPI and Django's ASGI handler now run through the test
  suite under `muructl run`, including WebSockets and lifespan state, an
  ASGI conformance suite pins the scope, message, lifespan and WebSocket
  rules, the Autobahn test suite checks the WebSocket wire protocol, and a
  how-to page covers the frameworks.


## 0.1.0a2 (2026-10-24)

### Added

- A first pre-release that serves requests. `muructl run` is development
  mode for one WSGI application: install `muructl` with `uv tool install
  muructl` and `muru` in the application's virtualenv, then `muructl run
  app:application`. The daemon, releases, traffic management and ASGI are
  still to come; the roadmap says when.
- Added HTTPS to `muructl run`: `--https` serves TLS on port 8443 (`--https-port`)
  next to HTTP, `--https-only` redirects HTTP to it, `--tls-cert` and
  `--tls-key` serve your own certificate, and otherwise a development
  certificate for localhost, 127.0.0.1, ::1 and any `--host` names is issued
  by your mkcert authority when one is installed or self-signed when not, kept
  in the user cache directory. HTTP/2 is negotiated over TLS, and the
  application sees `wsgi.url_scheme`, `HTTPS` and `muru.tls`.
- Added `--port` to `muructl run`, with `--port auto` picking free ports and
  printing them, and `--bind` now takes an address alone. A port already in
  use is reported with the process holding it, found through lsof or ss.
- Added `--print-config` to `muructl run`, which writes the declarative
  equivalent of the run (listeners, limits and the release) as the TOML that
  `muructl apply` will read, instead of running.
- Added `muructl run`, which serves one WSGI application behind a preloaded, forked worker pool over the framed worker link, on a fixed pool of long-lived application threads (the phase 0 spike).
- Added the WSGI conformance suite under `tests/conformance/wsgi`, run by `just conformance`, with cases taken from the mod_wsgi integration tests, and brought the bridge up to it: start_response validation and latin-1 header encoding, reason phrases, Content-Length enforcement, `wsgi.file_wrapper`, request headers with underscores dropped, and application exceptions that no longer end the worker.
- Each worker's stdout and stderr are pipes the host relays to the terminal, every line tagged with the worker's index (and the stream for stdout), so output from different workers can be told apart.
- Error responses the edge generates are content negotiated (HTML, an RFC 9457 problem document or plain text) and name their status, reason and request id; in development mode an application error's page includes the traceback. Every response carries X-Request-ID, a time-ordered identifier that is also muru.request_id in the environ.
- Flask and Django fixture applications (`tests/fixtures/apps`, installed from the `fixtures` dependency group) run through `muructl run` in the integration suite: forms and uploads, cookies and sessions, redirects, streaming, file responses, framework error pages and application errors.
- Preload modes for the host: auto runs the import check after preloading and falls back to per-worker imports when the import left behind a connected socket, pipe, thread or child process, naming the module responsible; true fails instead and false imports in every worker. Lifecycle hooks (on_load, pre_fork, post_fork, on_ready, on_drain, on_exit and on_reopen_logs on SIGUSR1) come from the module named by `muructl run --hooks`, and `muru.context()` reports the application, worker and process model.
- Request bodies without a Content-Length are read in full at the edge so the application sees CONTENT_LENGTH, bodies over the body size limit are refused with 413, hop-by-hop headers never reach the application, and a client sending Expect: 100-continue is told to continue only once the application starts reading, so a request it rejects first never carries its body across the link.
- The conformance suite sends malformed and ambiguous HTTP/1.1 requests, from conflicting framing headers to oversized heads, and checks that none reaches the application.
- The edge enforces the timeouts of section 7.6 with their defaults: a request waiting more than 5 s for a worker gets 503 queue_full, one without a response head after 300 s gets 504 deadline and its worker connection is dropped, a client that stops sending its body for 60 s gets 408, a worker that stops mid-response for 60 s is cut off, and request heads are limited to 64 KiB and 100 headers with 30 s to arrive. `muructl run --timeout NAME=DURATION` adjusts them.
- The edge serves HTTP/2 over cleartext by prior knowledge on the same listener as HTTP/1.1, with HTTP_HOST taken from the :authority pseudo-header. HTTP/2 over TLS follows with TLS support. `just bootstrap-conformance` fetches h2spec and the conformance suite runs it.
- `muructl run --reload` watches the project for changed Python files (with --reload-dir, --reload-include and --reload-exclude to adjust what), starts a new release from the same source when they change, switches the edge to it once its workers are ready and stops the previous release, which finishes the requests it is serving. A release whose import fails is reported and the previous one keeps serving. The watcher polls for now; inotify and FSEvents come with the daemon.
- `muructl run --single` serves in the host process with one thread and no fork, so a debugger attached to it sees the application. The hooks run there too, except pre_fork, which has no fork to precede.
- `muructl run` now takes a project directory, or nothing, as well as
  `module:callable`, reading the entrypoint and settings from the project's
  `[tool.muru]` table in pyproject.toml or from muru.toml; flags override the
  table. Added `--factory` (the entrypoint builds the application, per worker
  after post_fork, or in the host with `factory_phase = "host"`) and
  `--interface`, and renamed `--workers` to `--processes`. An ASGI entrypoint
  is refused with a message saying ASGI arrives in phase 2, and a worker that
  keeps failing at start stops the run instead of being forked forever.
- `muructl run` serves a read-only dashboard on loopback (http://127.0.0.1:8081/ by default, `--dashboard` to move or switch it off): workers and their in-flight requests, requests per second and latency percentiles over the last minute, recent requests and the host and worker output, refreshed by the embedded htmx.
- `muructl run` writes a compact access log line per request to the terminal, with the status, bytes, timings for queue, application and total, the worker and the request id. `--access-log` switches it off, limits it to errors and requests slower than `--slow`, or sends JSON lines to a file, and `--access-log-exclude` hides matching paths.

### Changed

- Added Python version classifiers to the muru distribution, so PyPI and the README badge show the supported versions, and moved both distributions from the Planning to the Alpha development status.
- `muructl run` now defaults to one process, reports `wsgi.multiprocess` as
  False for a single-process run, accepts request bodies up to 100 MB
  (`--max-body-size` changes it, and `muru.max_body_size` in the environ
  shows the limit in force) and warns once when the application sends a
  Strict-Transport-Security header, which browsers would apply to every
  server on localhost.
- `muructl run` now says at start-up and on the dashboard that it is
  development mode, refuses to bind an address outside loopback, and offers
  `--allow-public` to serve one anyway with the development behaviours off
  (no tracebacks in error pages, the 10 MB body limit, no dashboard unless
  asked for).

### Fixed

- A request's end of output is now signalled explicitly by the WSGI bridge. It was inferred from the drop of the start_response object, which Flask and Django keep alive in an exception's reference cycle after an error response, so the worker connection stayed busy until a later garbage collection and the next request on it hung.
- Fixed the dashboard's requests per second and error count under load: they
  were derived from the in-memory access log, which keeps the last 1000
  requests, so the rate could not rise past what that ring covered. The access
  log now keeps per-second counters for the last minute, which are exact at
  any rate.

### Documentation

- The documentation is also published as a PDF on Read the Docs. Mermaid diagrams appear there as source listings.


## 0.1.0a1 (2026-10-23)

### Added

- First pre-release, published to claim the muru and muructl names on PyPI and to exercise the release pipeline. Nothing runs yet: `muructl` reports that it is not implemented.
