Added connection limits: `max_connections` per listener with a fast 503
when full, `max_connections_per_client`, keep-alive timeouts (longer for
trusted proxies), `max_requests_per_connection` and the HTTP/2 stream
limit, in the daemon's `[limits]` table or as `muructl run` flags, with
the open and refused connections on `/metrics`.
