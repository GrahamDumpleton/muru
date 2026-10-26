# The daemon

```{admonition} Status
:class: note

The daemon serves one or more applications on one node, each deployed
from a source directory with the blue/green strategy, with rollback,
drains and an event log, through an edge process of its own, and it can
be stopped and started again without interrupting traffic. Applications,
releases, virtual hosts and routes can be declared in files and applied,
exported and compared, and TLS listeners serve each host's certificate.
What is still to come: canaries, worker groups, and the rest of the
roadmap.
```

`muructl run` is for development. For an application that stays up, start
the daemon once and deploy releases to it:

```
muructl daemon start
muructl deploy shop /srv/shop
```

The daemon serves HTTP on 127.0.0.1:8080 unless its configuration file
names listeners (see [virtual hosts and TLS](virtual-hosts.md)) or
`--listen` says otherwise, answering 503 until a release is ready. The
same file names the proxies to trust (see
[behind a proxy](behind-a-proxy.md)). The
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

A release that a deploy replaces is drained and then kept warm, with
its workers untouched, for a retention window of 30 minutes (`deploy
--retain` changes it; `--retain 0` stops it as soon as it has drained),
so `rollback` within the window is instant: traffic moves back to the
workers that were already serving. After the window, or when another
deploy supersedes it again, the retained release is stopped. One release
per application is retained at a time. `rollback` to a release that was
stopped starts it again first and moves traffic when it is ready; the
release it replaces is retained in turn. `drain` takes a release out of
traffic and stops it; draining the serving release leaves the application
answering 503 until the next deploy. `deploy --no-wait` returns once the
release is recorded, and `release wait` waits for a state.

A release whose host exits on its own is marked failed; if it was serving,
the daemon starts it again. A deploy whose release fails to start leaves
the previous release serving and reports the failure.

## Declaring applications in files

A file declares applications and their releases with the keys `muructl
deploy` takes, and virtual hosts and routes (see
[virtual hosts and TLS](virtual-hosts.md)), and `muructl apply -f FILE`
makes the daemon match it:

```toml
[app.shop.release.r1]
source = "/srv/shop"
entrypoint = "shop.wsgi:application"
python = "/srv/shop/.venv/bin/python"
processes = 2
threads = 8
```

```
muructl apply -f shop.toml
muructl apply -f config/ --dry-run
muructl apply -f config/ --prune
muructl config export > config/shop.toml
muructl config diff -f config/
```

Each `[app.NAME.release.ID]` table needs a `source`; a relative path is
taken from the file's own directory. The other keys are those of the
`[tool.muru]` table (`entrypoint`, `interface`, `factory`,
`factory_phase`, `python`, `processes`, `threads`, `sync_threads`,
`concurrency`, `preload` and `hooks`), and what a table leaves out comes
from the source's `[tool.muru]` table and then the defaults, exactly as
a deploy resolves them. A directory applies every `.toml` file in it, in
name order, as one configuration.

An apply creates the applications the files name, deploys each declared
release the daemon does not have (in id order, so the last declared
release of an application is the one that serves), creates or updates
the hosts and routes, and leaves anything already recorded as declared
alone. Releases are immutable: a
declared release whose settings differ from the recorded ones is an
error, and the file is applied only when it has no errors at all. Unknown
keys, invalid names, a missing source directory and a source without an
entrypoint are errors too. `[[listener]]` entries and the `[limits]`
table belong in the daemon's own configuration file; in an applied file
they are reported as warnings and not applied.

`--dry-run` prints the plan and changes nothing. `--prune` also removes
the routes, hosts, releases and applications the files do not contain,
in that order: a running release drains and stops first. `--json` writes
the report as JSON. The
exit code is 0 when there is nothing to do or everything applied, 2 for
a dry run with changes planned, and 1 for errors.

`muructl config export` writes desired state as a file of this form, with
a header naming the daemon's version, the node, the time and the state
version. It writes the release that serves each application, every key
spelled out, so applying the file back to the daemon it came from changes
nothing; `--all-releases` writes every release, `--app NAME` one
application, and `--json` the same document as JSON. A
[rollout](rollouts.md) under way is not part of the file; with
`--resolve-rollouts` it is written as where it stands, the candidate
declared beside the serving release and the application's routes
splitting traffic between them at the candidate's current share.
`muructl config diff -f FILE` compares a file with desired state in
both directions, listing what the file would add and what the daemon
holds that the file does not, with exit code 2 when they differ.

`muructl config show [APP] --effective` writes the same document headed
by each serving release's settings and where each came from: the
release table of an applied file or the deploy's flags, the source's
`[tool.muru]` table, the source's `.venv` for the interpreter, the
previous release, or the default:

```console
$ muructl config show shop --effective
# Muru effective state: muructl 0.1.0 on web1, 2026-11-02T02:10:00Z, state version 812.
# No active overlay changes this configuration.

# The releases' settings, and the layer each came from:
#   shop@r3: entrypoint = shop.wsgi:application ([tool.muru])
#   shop@r3: python = /srv/shop/.venv/bin/python (the source's .venv)
#   shop@r3: processes = 4 (deploy flags)
#   shop@r3: threads = 8 (the previous release)
#   shop@r3: sync_threads = 8 (default)
...
```

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
addresses to serve, each with `tls = true` when the edge terminates TLS
on it, and its `[limits]` table sets the body size limit and timeouts,
with the same keys `muructl run --print-config` writes.

## Running it

`muructl daemon start` starts the daemon in the background, logging to
`daemon.log` in the log directory, and waits until it answers; `muructl
up` does the same and is harmless when one already runs. `muructl daemon
status` and `muructl status` show the daemon, its edge process, its
listener and its locations. `--metrics ADDRESS`, or a `[metrics]` table
in the configuration file, serves Prometheus metrics on an internal
address; the [metrics guide](metrics.md) lists them. `--dashboard
ADDRESS`, or a `[dashboard]` table, serves the [dashboard and JSON
gateway](dashboard.md) on a loopback address, read-only unless the
table turns its actions on. An `[overlay_webhook]`
table serves the [signed webhook](overlays.md#the-webhook) that
monitoring systems activate and end overlays with.

The daemon binds the listening sockets itself and hands them to an edge
process started from the same executable, which carries the traffic;
the daemon is never on the request path. A daemon that runs as root
starts the edge, and the applications that name no user, as the service
user, `muru` unless `--user` or the `[daemon]` table names another, and
refuses to start when that account does not exist; the [per-application
users guide](users.md) has the details. If the edge exits, the daemon
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
its workers with it, and the next daemon starts its release again.

## Upgrading

Installing a new version of Muru changes nothing until `muructl daemon
upgrade`. The daemon then re-executes itself from the installed `muructl`
(the one its command name finds on the `PATH`, or the path it was started
with), keeping its process id and its children, and the new version takes
the hosts and the edge over from their records, exactly as a restarted
daemon does. It then starts a new edge from its own version on the same
listening sockets, gives it the routing in force, and tells the old edge
to stop accepting: that edge finishes the requests in flight, closes its
idle keep-alive connections (with GOAWAY on HTTP/2) and exits. No
connection is refused and no request fails in between; WebSocket
connections still open on the old edge are closed when it exits. The
command waits until the upgraded daemon answers with its new edge and
prints the versions and process ids involved. The daemon checks that the
installed binary runs before it commits, and the `daemon.upgraded` and
`edge.replaced` events record what happened.

Releases keep the runtime library they started with; a new release picks
up the installed one. Under systemd the service's main process is
unchanged by an upgrade, so no unit restart is involved.
`muructl daemon run` runs it in the foreground, for a service manager or
a terminal; SIGTERM or an interrupt detaches it the same way, and
`muructl daemon stop --timeout` bounds how long the command waits for the
daemon to exit. `muructl daemon reload`, or SIGHUP, re-reads the
configuration file for the `[proxy]`, `[compression]`, `[error_pages]`
and `[error_override]` tables. The [systemd
guide](systemd.md) has the unit and what it does. Commands find the
daemon through its admin socket: `--socket` or `MURU_SOCKET`, else the
socket in the runtime directory.

The access log is written as JSON lines to `access.log` in the log
directory, `muructl logs` shows or follows it and each release's output,
and `muructl logs reopen` works with logrotate; the [logging
guide](logging.md) has the details. The events `muructl events` shows are
kept in the state store, and `[[events.sink]]` entries deliver them to
webhooks; the [events guide](events.md) has the details.
