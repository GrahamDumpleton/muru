A WebSocket's request now ends when the application accepts it: its
access log line is written then, with the handshake's time, and a second
line with the same request id is written when it closes. Its lifetime no
longer goes into `muru_request_duration_seconds`, the latency charts or
the rollout gates, and it no longer counts in `muru_requests_in_flight`
once accepted.
