Added the edge's retry of a request no worker has read: when the worker
chosen cannot be connected to, or closes its connection before the
request head reaches it, as a worker that is stopping does, the request
goes to another worker. Retries are counted in `muru_request_retries_total`
and marked `retried` in the access log; a second failure is 502
`worker_unavailable`. A request a worker has read is never sent again.
