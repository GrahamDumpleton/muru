Added `muructl run`, which serves one WSGI application behind a preloaded, forked worker pool over the framed worker link, on a fixed pool of long-lived application threads (the phase 0 spike).
