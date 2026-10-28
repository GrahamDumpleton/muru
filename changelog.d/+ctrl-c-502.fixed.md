Fixed Ctrl-C on `muructl run` answering requests in the stopping window
with 502 `worker_crashed`, and the host logging its stop twice: the edge
now stops accepting and finishes the requests it holds before the host is
told to stop, and only `muructl` tells it, since the host and its workers
no longer share the terminal's process group.
