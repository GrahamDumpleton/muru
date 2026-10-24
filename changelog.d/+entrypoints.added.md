`muructl run` now takes a project directory, or nothing, as well as
`module:callable`, reading the entrypoint and settings from the project's
`[tool.muru]` table in pyproject.toml or from muru.toml; flags override the
table. Added `--factory` (the entrypoint builds the application, per worker
after post_fork, or in the host with `factory_phase = "host"`) and
`--interface`, and renamed `--workers` to `--processes`. An ASGI entrypoint
is refused with a message saying ASGI arrives in phase 2, and a worker that
keeps failing at start stops the run instead of being forked forever.
