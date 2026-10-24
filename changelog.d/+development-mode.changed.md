`muructl run` now says at start-up and on the dashboard that it is
development mode, refuses to bind an address outside loopback, and offers
`--allow-public` to serve one anyway with the development behaviours off
(no tracebacks in error pages, the 10 MB body limit, no dashboard unless
asked for).
