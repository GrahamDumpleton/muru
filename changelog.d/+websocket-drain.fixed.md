A worker told to stop no longer leaves its open WebSockets to be cut off:
after the new `websocket_drain_grace` timeout (none by default) it closes
each with 1001, which the application sees as `websocket.disconnect` and
the client as a close it can reconnect after, where before the client got
1011 once the worker was killed or gave up 30 seconds later. Workers are
now killed `websocket_drain_grace` plus `shutdown_timeout` after being
told to stop, as the timeouts guide says, and a drained release's workers
no longer outlive the release being reported stopped.
