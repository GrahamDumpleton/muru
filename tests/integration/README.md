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

Run with `just test-integration`. Scenarios that need Docker, network, root or
a long time carry markers and are skipped by default. Helpers for starting
muructl and talking to it are in `tests/support`.
