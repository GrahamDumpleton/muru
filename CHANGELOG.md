# Changelog

All notable changes to Muru are recorded here. The file is assembled from
fragments in `changelog.d/` by towncrier when a release is prepared; do not
edit the released sections by hand.

Muru follows semantic versioning. Before 1.0, minor versions may contain
breaking changes, which are listed under each release.

<!-- towncrier release notes start -->

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
