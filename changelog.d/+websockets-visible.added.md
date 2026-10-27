Open WebSockets are now visible: the `muru_websocket_connections` gauge,
`muru_websockets_closed_total` by who closed and how, and the
`muru_websocket_duration_seconds` histogram; `muructl websockets`, which
counts open sockets by endpoint with identifiers in paths folded together,
or lists them with `--list`; the count in `muructl status` and
`muructl release show`; and the dashboards' WebSockets table, tile, chart
and `/v1/websockets`.
