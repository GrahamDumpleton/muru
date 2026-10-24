Fixed the dashboard's requests per second and error count under load: they
were derived from the in-memory access log, which keeps the last 1000
requests, so the rate could not rise past what that ring covered. The access
log now keeps per-second counters for the last minute, which are exact at
any rate.
