Added charts over time to the dashboards (requests, errors and latency
over the last hour or day), served as JSON on `/v1/history` and streamed
with Server-Sent Events, and the daemon's events stream on
`/v1/events/stream`, which the page uses to show events as they happen.
