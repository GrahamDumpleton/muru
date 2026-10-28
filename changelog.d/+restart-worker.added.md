Added `muructl restart APP[@RELEASE] --worker N`, which replaces one worker
with a fresh one the way a recycle does, with `--pid` to name it by process
id, `--all` for every worker in turn, `--force` to take it out of routing
and kill it at once with its stacks in the log, and `--no-wait`. The event
is `worker.restarted`. A restart resets the worker's state but loads no new
code; new code is a new release.
