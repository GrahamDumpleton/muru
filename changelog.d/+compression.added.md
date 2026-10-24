Added response compression at the edge: responses the client accepts are
compressed with br, zstd or gzip, for the configured content types and
sizes, from the daemon's `[compression]` table or by default under
`muructl run` (off with `--no-compression`).
