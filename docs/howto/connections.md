# Connections and keep-alive

```{admonition} Status
:class: note

Limits on open connections per listener and per client, keep-alive
timeouts with a longer one for trusted proxies, a request limit per
connection and the HTTP/2 stream limit, set in the daemon's `[limits]`
table or by `muructl run` flags. Eviction of idle connections under
pressure, the shrinking idle timeouts, per-listener settings, HTTP/2
idle closure and Unix socket listeners are still to come.
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `max_connections` | 65536 | open connections per listener; a client beyond it gets a fast `503` with `Retry-After` while the overload reserve (one percent of the limit, at least one) lasts, and is closed beyond that |
| `max_connections_per_client` | 256 | open connections per client address (IPv6 grouped by /64), applied first, so one client cannot crowd others out; a connection over the share is closed at once; `0` disables it; trusted proxies are exempt |
| `keep_alive_timeout` | 20s | how long an idle HTTP/1.1 connection waits for its next request before it is closed |
| `keep_alive_timeout_proxy` | 75s | the same for peers in `[proxy] trusted`, so it exceeds the idle timeouts of common load balancers |
| `max_requests_per_connection` | 0 | requests served on one HTTP/1.1 connection before it is closed with `Connection: close`; `0` leaves it open |
| `http2.max_concurrent_streams` | 128 | concurrent streams per HTTP/2 connection |

Under the daemon the keys go in the `[limits]` table of its
configuration file and are read when the daemon starts:

```toml
[limits]
max_connections = 10000
max_connections_per_client = 100
keep_alive_timeout = '15s'
keep_alive_timeout_proxy = '90s'
max_requests_per_connection = 1000

[limits.http2]
max_concurrent_streams = 256
```

`muructl run` takes `--max-connections`, `--max-connections-per-client`,
`--max-requests-per-connection`, and `--timeout keep_alive=15s` or
`--timeout keep_alive_proxy=90s`, and `--print-config` writes them.

The keep-alive timer is the one that bounds reading a request head on
that connection, so on an untrusted connection the shorter of
`keep_alive_timeout` and `header_read_timeout` applies to both, and on a
trusted proxy's connection `keep_alive_timeout_proxy` does. A client
whose idle connection was closed between requests retries on a new one,
which browsers and HTTP libraries do.

Each listener's open connections and what it refused are served on
`/metrics` as `muru_connections_open` and
`muru_connections_refused_total` by reason: `client_limit`, `overloaded`
(answered `503`) and `paused` (closed beyond the reserve); the
[metrics guide](metrics.md) lists them.
