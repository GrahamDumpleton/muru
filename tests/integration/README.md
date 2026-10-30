# Integration tests

pytest scenarios driving muructl against the fixture applications in
`tests/fixtures/apps`. The scenarios are listed in section 15.2 of the
design specification and are added as the features they cover land:

- `test_spike.py`: a preloaded, forked WSGI application behind the edge,
  from hello world to keep-alive, concurrency, thread-locals, contexts and
  unread request bodies.

- `test_lifecycle.py`: preload modes with the import check, per-worker
  imports, and the lifecycle hooks from on_load to on_exit, including
  on_reopen_logs on SIGUSR1.

- `test_access_log.py`: the access log on the terminal, off, for errors
  and slow requests only, with excluded paths, and as JSON lines in a file.

- `test_reload.py`: `--reload` starting a new release when a source file
  changes, keeping the previous one when the new import fails, and ignoring
  files outside the patterns.

- `test_limits.py`: the queue, deadline, body idle and header read
  timeouts, shortened with `--timeout`.

- `test_frameworks.py`: the Flask and Django fixtures, each exercised the
  way its own test client would be, over HTTP.

- `test_dashboard.py`: the read-only dashboard on loopback, its fragments
  and the embedded htmx.

- `test_load.py`: deploys, rollbacks, drains and a daemon restart under
  sustained load from `support.load`, with zero failed requests; the
  minute-long cycle carries the slow marker.

- `test_shutdown.py`: what SIGTERM means to the daemon: a detach by
  default, the shutdown under the `[shutdown]` table, with its delay,
  its budget and the one sequence `muructl daemon stop --shutdown` runs.

- `test_subreaper.py` (Linux only): the host collecting and stopping
  what a killed worker, or a stopping one, leaves behind.

- `test_containers.py` (marked requires_docker): muructl as a container's
  main process, in an image built from the working tree with
  `images/test/Dockerfile`: PID 1 as the built-in init, no zombies after a
  hard timeout, and `docker stop` draining both `muructl run` and the
  daemon inside the budget.

- `test_tutorial_*.py`: one per tutorial under `docs/tutorials`, each
  running the tutorial's own code from `docs/tutorials/code` through the
  page's steps, so a change that breaks a step fails here rather than
  for a reader.

Run with `just test-integration`. Scenarios that need Docker, network, root or
a long time carry markers and are skipped by default. Helpers for starting
muructl and talking to it are in `tests/support`.
