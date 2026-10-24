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

Run with `just test-integration`. Scenarios that need Docker, network, root or
a long time carry markers and are skipped by default. Helpers for starting
muructl and talking to it are in `tests/support`.
