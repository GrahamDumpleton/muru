Added per-group timeouts and memory policy: a worker group's `timeouts`
table may set the soft, hard, hard drain grace, loop stall and GIL stall
timeouts, and its `memory` table any memory limit, each key the release's
when absent. `shutdown_timeout` and `websocket_drain_grace` stay with the
release.
