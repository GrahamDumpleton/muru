# Settings for muructl run

`muructl run` takes what the application is from the project's
`[tool.muru]` table and how it serves from the flags. The flags a developer
types every time, the port, `--https`, `--host`, the reload patterns, the
access log, can live in the project too, in a table of their own, and a
developer can keep a copy of that table beside the project for one
machine alone.

## The run table

```toml
[tool.muru]
entrypoint = "shop.wsgi:application"
processes = 2

[tool.muru.run]
port = 9000                     # or "auto"
https = true
https_only = false
https_port = 9443
tls_cert = "certs/dev.pem"      # relative to the project, as the flags are
tls_key = "certs/dev-key.pem"
hosts = ["shop.localhost"]
reload = true                   # the default in development mode
reload_dirs = ["shop", "templates"]
reload_include = ["**/*.html"]
reload_exclude = ["**/*.py"]
access_log = "errors"           # terminal, off, errors, or a file path
access_log_exclude = ["/static/*"]
slow = "500ms"
dashboard = "127.0.0.1:8082"    # or "off"
compression = false
```

In a `muru.toml` the same keys go in a `[run]` table at the top level.
Each key is a flag of `muructl run`, named in snake case and taking the
same values; `compression = false` stands for `--no-compression`. With
the table above, `muructl run` on its own is the whole command in place
of:

```
muructl run shop.wsgi:application --https --host shop.localhost --port 9000 --reload-include '**/*.html' --access-log errors
```

A key belongs in the table when it describes how this checkout serves
rather than what the application is. `processes`, `threads`, the
timeouts, the memory policy and the static directories describe the
application and stay in `[tool.muru]`. `--bind` and `--allow-public` stay
on the command line, so that serving beyond loopback is something typed
each time; so do `--python` and `--chdir`, which describe the machine and
the invocation, `--single` and the link flags, and `--max-body-size`,
which has its production counterpart in the host limits.

The table applies whenever the project is found: as the argument, with
`--chdir`, or in the current directory, with an entrypoint on the command
line too. The daemon ignores it, so `muructl deploy` of the project
neither reads nor rejects it, and `--print-config` leaves it out while
writing the listeners and TLS it resolved.

## Your own machine: muru.local.toml

A `muru.local.toml` beside the project's `pyproject.toml` or `muru.toml`
holds a `[run]` table with the same keys, for the machine it is on:

```toml
[run]
port = 9100
dashboard = "off"
```

It holds the `[run]` table and nothing else; a key outside it is refused
with a message naming the committed file where the application's settings
belong. It is never committed: `muructl run` says once per run that it is
using the file, and if the project's `.gitignore` does not cover it, the
start-up line says so. `muructl deploy` does not read it.

## What wins

Highest first: a flag, then `muru.local.toml`, then `[tool.muru.run]`,
then the defaults. Lists add across the layers, so `hosts` in the table,
`hosts` in the local file and `--host` on the command line are all
served; the reload patterns and the access log excludes add the same way.
A boolean a file turns on has a flag to turn it off for one run:
`--no-reload`, `--no-https` and `--no-https-only`.

## Reload

A development run reloads on change without a flag: it watches the
project and starts a new release when a source file changes, and the
start-up line says so beside the directories and patterns watched.
`--no-reload` turns it off for one run, and `reload = false` in the run
table or in `muru.local.toml` for good; `--reload` turns it back on over
a file that turned it off. Under `--allow-public` the run does not reload
unless `--reload` is given, and the daemon never reloads.
