# Managed HTTP servers

```{admonition} Status
:class: note

Releases with `kind = 'http'`: a command Muru starts, probes, supervises
and stops, with profiles for gunicorn, uvicorn, Granian, mod_wsgi-express
and generic servers, routed to like an upstream so traffic policies,
rollouts, drains and rollbacks apply to it. Instance autoscaling and
socket activation are still to come.
```

A managed HTTP server release runs an ordinary HTTP server, such as the
gunicorn or uvicorn an existing deployment already uses, instead of
Muru's own workers. Muru starts the server's instances, waits until they
answer, routes requests to them the way it routes to an
[upstream](upstreams.md), restarts an instance that exits, and stops them
when the release drains. The release is a release like any other: it
serves or is held, takes a share of a route's traffic, is canaried
against, drained, kept warm and rolled back to. The intended use is a
cutover: run the current deployment unchanged under Muru first, then move
traffic to native workers of the same application a step at a time.

## Declaring one

```toml
[app.legacy.release.r9]
kind = 'http'
profile = 'gunicorn'
command = ['gunicorn', '--workers', '4', '--bind', '{bind}', 'legacy.wsgi:application']
cwd = '/srv/legacy'
env = { DJANGO_SETTINGS_MODULE = 'legacy.settings' }
bind = 'unix'
instances = 1
readiness = { path = '/healthz', timeout = '60s' }
```

| Key | Default | Meaning |
| --- | --- | --- |
| `kind` | `workers` | `http` runs a server from `command` instead of workers from an entrypoint |
| `profile` | required | `gunicorn`, `uvicorn`, `granian`, `mod_wsgi-express` or `generic`; fixes how the server binds and how it is stopped |
| `command` | required | the server's command line; a bare name is found on `PATH`, a relative path with a directory in it is taken from `cwd` |
| `cwd` | `source` | the directory the command runs in; one of the two is required |
| `bind` | `unix` | `unix`, a socket per instance under the release's run directory, or `port`, a free loopback port per instance |
| `instances` | `1` | how many instances of the command run, each with its own address |
| `readiness` | none | `path` is probed with a GET every 100 ms until it answers 2xx or 3xx; without it any HTTP response makes the instance ready. `timeout` (default `60s`) is how long the instances get |
| `stop_signal` | the profile's | the signal that stops an instance gracefully: `TERM`, `INT`, `QUIT`, `HUP`, `USR1`, `USR2` or `WINCH`; required by `generic` |
| `env` | none | environment variables and secret references, as for a workers release |
| `timeouts.shutdown_timeout` | `5s` | how long an instance told to stop gets before it is killed |

The same release is deployed from the command line with `--server` and
the command after `--`:

```console
$ muructl deploy legacy /srv/legacy --server gunicorn --readiness /healthz -- \
    gunicorn --workers 4 --bind '{bind}' legacy.wsgi:application
muru: release legacy@r9 is ready and serving
```

`--bind`, `--instances`, `--readiness-timeout` and `--stop-signal` set
the other keys, and `--env` and `--secret` the environment. The worker
settings of a release (`entrypoint`, `processes`, `threads`, `preload`,
`groups`, `mounts`, `scaling`, `memory`, the soft and hard timeouts and
the rest) do not apply to a managed server and are refused.

## Binding

Each instance gets an address of its own: a Unix socket under the
release's run directory, or a loopback port. The command says where the
server takes it:

- `{bind}` is replaced by the address in the server's own form: for
  gunicorn and generic `unix:/path` or `127.0.0.1:PORT`, for uvicorn and
  Granian the socket path or `127.0.0.1:PORT`.
- `{socket}` is replaced by the socket path and `{port}` by the port
  number, for servers with a flag of their own for each.
- A command naming none of them gets the profile's flags appended:
  `--bind ADDRESS` for gunicorn, `--uds PATH` or
  `--host 127.0.0.1 --port PORT` for uvicorn and Granian, and
  `--host 127.0.0.1 --port PORT` for mod_wsgi-express, which binds a
  port only. A generic command must name `{bind}` or read the
  environment.

Every instance also has `MURU_HTTP_BIND` (the address as `unix:/path` or
`127.0.0.1:PORT`), `PORT` when it binds a port, and `MURU_INSTANCE`, its
index from 0, in its environment.

```toml
[app.api.release.r3]
kind = 'http'
profile = 'uvicorn'
command = ['.venv/bin/uvicorn', 'api.main:app', '--workers', '2']
cwd = '/srv/api'
bind = 'port'
instances = 2
```

## Readiness, supervision and stopping

The release is `warming` until every instance answers its probe, and
`failed` if one exits first or the timeout passes, with the instance and
the reason in the release's message. An instance that exits later is
started again after a wait that doubles with each exit in a row, from one
second up to thirty, and starts over once an instance has run for a
minute; the exit and the restart are `server.exited` and `server.started`
events, and a start that fails is `server.failed`. While an instance is
down its requests go to the others, or are answered 502
`upstream_failed` when there is none, and `muru_upstream_health` reports
each instance by address under the release's name. The instance count is
fixed: `muructl scale` refuses a managed server.

Draining stops new requests at the edge, waits for those in flight, then
sends each instance the profile's stop signal (`TERM` for every profile
unless `stop_signal` says otherwise) and kills one still there after
`shutdown_timeout`. A superseded release is kept warm as a workers release
is, with its instances running, so a rollback to it is instant.

## Requests, logs and the daemon

Requests reach the server re-serialised as they reach an
[upstream](upstreams.md): the client's headers less hop-by-hop ones, with
`X-Forwarded-For`, `X-Forwarded-Proto`, `X-Forwarded-Host`, `X-Real-IP`
and `X-Request-ID` added, bodies re-framed with a `Content-Length`, and
the path as the client sent it. The server's `X-Muru-Redirect` response
header is an [internal redirect](internal-redirects.md), as from an
application's workers. WebSocket upgrades are relayed. The access log
records each request under the application and release with `http` as
its group and the instance's address as its mount.

The instances' standard output and error go to the release's log file,
shown by `muructl logs APP`, and the daemon rotates that file by copying
it and truncating it in place, since the server cannot be told to reopen
it. A daemon that stops without `--shutdown`, or upgrades itself, leaves
the instances running and the next daemon takes them over from the
record in the release's run directory. `muructl release list` shows a
managed server as its instance count and profile in the workers column.

## Cutting over

```console
$ muructl deploy legacy /srv/legacy --server gunicorn -- gunicorn legacy.wsgi:application
muru: release legacy@r1 is ready and serving
$ muructl deploy legacy /srv/legacy --hold
muru: release legacy@r2 is ready and held: not serving
$ muructl rollout canary legacy@r2 --steps 5,25,50 --hold 10m --wait
muru: rollout 1 promoted: every step passed its gates
$ muructl drain legacy@r1
```

The first release runs the deployment as it is. The second is the same
application on Muru's workers, held without traffic and checked through a
preview. The [canary](rollouts.md) moves clients to it step by step,
judged against the gunicorn release, and rolls back to it when a gate
fails. Once promoted, the gunicorn release drains and is kept warm until
it is drained for good, and nothing in the application's routes or hosts
changed along the way.
