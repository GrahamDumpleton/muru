# The daemon

```{admonition} Status
:class: note

The daemon serves one or more applications on one node, each deployed
from a source directory with the blue/green strategy, with rollback,
drains and an event log, through an edge process of its own, and it can
be stopped and started again without interrupting traffic. What is still
to come: warm retention of drained releases, virtual hosts and TLS,
declarative configuration, canaries, and the rest of the roadmap.
```

`muructl run` is for development. For an application that stays up, start
the daemon once and deploy releases to it:

```
muructl daemon start
muructl deploy shop /srv/shop
```

The daemon serves HTTP on 127.0.0.1:8080 unless its configuration file or
`--listen` says otherwise, answering 503 until a release is ready. The
first deploy creates the application and its first release; each later
deploy creates the next release, starts it beside the one that serves,
moves traffic to it once every worker is ready, and then drains and stops
the previous one. Requests in flight on the previous release finish
first. No request is refused during the switch.

## Releases

A release is a source directory and the settings fixed at creation: the
entrypoint, interface, interpreter, processes, threads and preload mode,
read from the directory's `[tool.muru]` table unless a flag overrides
them, exactly as `muructl run` reads them. Release ids are `r1`, `r2` and
so on unless `--release` names one. The interpreter is the source
directory's `.venv` when it has one, otherwise the previous release's,
otherwise `--python`.

```
muructl release list shop
muructl release wait shop@r3
muructl rollback shop
muructl drain shop@r2
muructl events --follow
```

`rollback` starts the previous release again if it was stopped, moves
traffic to it when it is ready and drains the one that served. `drain`
takes a release out of traffic and stops it; draining the serving release
leaves the application answering 503 until the next deploy. `deploy
--no-wait` returns once the release is recorded, and `release wait` waits
for a state.

A release whose host exits on its own is marked failed; if it was serving,
the daemon starts it again. A deploy whose release fails to start leaves
the previous release serving and reports the failure.

## Where things live

The daemon keeps its state store, run directory and logs in the user
locations for the platform, or the system locations with `--system`:

| What | Linux, user | macOS, user | System |
| --- | --- | --- | --- |
| State store | `~/.local/state/muru` | `~/Library/Application Support/muru` | `/var/lib/muru` |
| Admin socket and run directories | `$XDG_RUNTIME_DIR/muru` | `/tmp/muru-UID` | `/run/muru` |
| Logs | `~/.local/state/muru/logs` | `~/Library/Logs/muru` | `/var/log/muru` |
| Configuration file | `~/.config/muru/muru.toml` | `~/Library/Application Support/muru/muru.toml` | `/etc/muru/muru.toml` |

`--state-dir`, `--run-dir`, `--log-dir`, `--cache-dir` and `--config`
override each, as do `MURU_STATE_DIR`, `MURU_RUN_DIR`, `MURU_LOG_DIR`,
`MURU_CACHE_DIR` and `MURU_CONFIG`, and the directories systemd provides
through `StateDirectory=` and its siblings are used when set. `muructl
daemon run` prints every location and which rule chose it. The socket
path is limited to about a hundred bytes, so a deep `--run-dir` is
refused with a message saying so.

The configuration file is optional. Its `[[listener]]` entries name the
addresses to serve and its `[limits]` table sets the body size limit and
timeouts, with the same keys `muructl run --print-config` writes.

## Running it

`muructl daemon start` starts the daemon in the background, logging to
`daemon.log` in the log directory, and waits until it answers; `muructl
up` does the same and is harmless when one already runs. `muructl daemon
status` and `muructl status` show the daemon, its edge process, its
listener and its locations.

The daemon binds the listening socket itself and hands it to an edge
process started from the same executable, which carries the traffic;
the daemon is never on the request path. If the edge exits, the daemon
starts another on the same socket with the same routing, and connections
made in between wait in the socket's backlog rather than being refused.
The edge's own output goes to `edge.log` in the log directory, and each
release's host writes to `apps/APP/RELEASE.log` there.

## Stopping and restarting

`muructl daemon stop` detaches: the daemon exits, and the edge and every
host keep serving exactly as they were. The next `muructl daemon start`
(or `up`, or a service manager restarting the unit) finds them through
the records in the runtime directory, takes them over, and carries on,
so a daemon restart or upgrade never costs a request. `muructl daemon
stop --shutdown` drains every release, stops the hosts and the edge, and
then exits, for taking a node out of service.

A host whose daemon is gone keeps its workers; a host that dies takes
its workers with it, and the next daemon starts its release again. `muructl daemon run` runs it in the foreground, for a service
manager or a terminal. Commands find the daemon through its admin socket:
`--socket` or `MURU_SOCKET`, else the socket in the runtime directory.

The access log is written as JSON lines to `access.log` in the log
directory, and the events `muructl events` shows are kept in the state
store.
