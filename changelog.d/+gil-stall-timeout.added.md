Added the GIL stall timeout: every worker, WSGI or ASGI, now has a native
thread that attaches to the interpreter every half second, and a worker in
which no thread has run Python for `gil_stall_timeout` (30 s by default,
`--timeout gil_stall=`) is replaced as for the hard timeout, with a
`worker.gil_stalled` event and its stacks in the log. The host also
replaces a worker whose reports have stopped for that long, such as one
stopped by a signal.
