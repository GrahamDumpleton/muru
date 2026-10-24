Added worker recycling and the hard timeout: `max_requests` and
`max_lifetime` replace a worker with a fresh one on a new socket with no
request failed, and a request past `--timeout hard=DURATION` has its
worker taken out of routing, its stacks written to the log and the worker
replaced, with `worker.recycled` and `worker.timeout_killed` events.
