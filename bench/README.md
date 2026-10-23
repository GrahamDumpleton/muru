# Benchmarks

Scenarios and scripts for the performance comparisons in section 15.4 of the
design specification: small and large responses, tiny-chunk streaming, large
uploads, many idle keep-alive connections, WebSocket echo and mixed mounts,
against gunicorn (sync and gthread), uvicorn, Granian and mod_wsgi-express in
three configurations.

Results are stored as JSON under `results/` (ignored by git) and compared
with the last main-branch results in CI. Run on Linux only for representative
numbers.
