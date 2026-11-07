# The daemon

The daemon serves one or more applications on one node, each deployed
from a source directory, with rollback, drains and an event log, and can
be stopped and started again without interrupting traffic. This page
covers starting it and deploying; the other guides cover what it does
with a deployed application, from [rollouts](rollouts.md) to
[scaling](scaling.md).

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

A release reaches the daemon in one of two ways. `muructl deploy` makes
one now, from a source directory and the flags given. Where a record of
each release is kept in version control, `muructl render` writes the
release as a file from the same flags, and `muructl apply` makes the
daemon match the file, as [declaring applications in
files](#declaring-applications-in-files) describes.

## Releases

A release is a source directory and the settings fixed at creation: the
entrypoint, interface, interpreter, processes, threads, preload mode,
timeouts, memory policy, environment and the rest a flag can set, read
from the directory's `[tool.muru]` table unless a flag overrides them,
exactly as `muructl run` reads them. Release ids are `r1`, `r2` and so
on unless `--release` names one.

A deploy takes what neither its flags nor the source's table give from
the release before it, so a `--timeout soft=3s` or `--env DEBUG=1` given
once stays in force through later deploys until a flag or the table
changes it. Environment variables and secrets carry by name. The tables
in `[tool.muru]`, worker groups, mounts, logging, scaling and error
pages, come from the source alone, so a source that drops a group drops
it. The interpreter is the source directory's `.venv` when it has one,
otherwise the previous release's, otherwise `--python`. `--fresh` deploys
from the flags, the source and the defaults alone, ignoring the previous
release.

`deploy --dry-run` asks the daemon what the deploy would do and changes
nothing. It prints the id the release would take, each setting with the
layer it would come from, whether an environment would be built, and
what becomes of the release that serves:

```console
$ muructl deploy shop /srv/shop --processes 4 --dry-run
muru: plan (nothing changed): release shop@r4, shop.wsgi:application 4x8 from /srv/shop
  entrypoint = shop.wsgi:application    ([tool.muru])
  python = /srv/shop/.venv/bin/python   (the source's .venv)
  processes = 4                         (deploy flags)
  threads = 8                           ([tool.muru])
  sync_threads = 8                      (default)
  max_requests = 5000                   (the previous release)
  ...
  settings nothing else gives come from shop@r3; --fresh takes none
  the release would take over from shop@r3, which would stay warm for 1800s with 1 worker to a group
```

When the deploy would be refused, the plan lists why instead: a release
id already taken, an overlay that freezes the application's deploys, a
setting that fails its check, or a release the node's memory budget has
no room for. An interpreter the node does not have is reported and not
installed. The exit code is 0 when the deploy would go ahead and 1 when
it would be refused, and `--json` prints the plan as JSON.

```
muructl release list shop
muructl release show shop@r3
muructl release wait shop@r3
muructl rollback shop
muructl drain shop@r2
muructl restart shop --worker 1
muructl events --follow
```

A release that a deploy replaces is drained and then kept warm for a
retention window of 30 minutes (`deploy --retain` changes it; `--retain
0` stops it as soon as it has drained), with one worker in each of its
groups (`deploy --retain-workers` changes it; `--retain-workers 0` keeps
the host alone), so `rollback` within the window is instant: traffic
moves back to the warm workers at once, and the groups grow back to
their counts by forking from the host. After the window, or when another
deploy supersedes it again, the retained release is stopped. One release
per application is retained at a time. `rollback` to a release that was
stopped starts it again first and moves traffic when it is ready; the
release it replaces is retained in turn. `drain` takes a release out of
traffic and stops it; draining the serving release leaves the application
answering 503 until the next deploy. `deploy --no-wait` returns once the
release is recorded, and `release wait` waits for a state. `restart`
replaces one worker of a release, or each in turn, as the [recycling
guide](recycling.md#restarting-a-worker-by-hand) describes.

`release show` prints one release: its state, source, interpreter,
entrypoint and sizes, and its worker groups and mounts:

```
release      shop@r3
state        ready, serving
source       /srv/shop
python       /srv/shop/.venv/bin/python
entrypoint   shop.wsgi:application, WSGI (detected)
workers      2 processes of 8 threads and an event loop of concurrency 1000 with 8 sync threads
group reports /reports/*: 6 processes of 1 thread and an event loop of concurrency 1000 with 8 sync threads
mount ws     /ws from shop.asgi:websocket_app, ASGI (detected)
```

While WebSockets are open on a release's workers, a `websockets` line
says how many; [watching WebSockets](asgi.md#watching-websockets) shows
how to list them.

An interface declared `auto` is shown as the host detected it when it
imported the application. When the workers import it themselves, with
preload off or a factory called in each worker, it is `interface auto,
not yet detected`, and the release's log (`muructl logs shop`) has each
worker's line saying what it serves. The sizes shown are the ones the
interfaces use: threads for WSGI, the event loop for ASGI and gRPC.

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
taken from the file's own directory, as every other path in the file is:
a route's or an overlay's `static` directory, `sendfile_roots`, a host's
certificate and key, a host's and a route's error page files, an
upstream's `tls.ca` and a client class's lists. A release's error page
files are the one exception, taken from its `source`. The other keys are
those of the
`[tool.muru]` table (`entrypoint`, `interface`, `factory`,
`factory_phase`, `python`, `processes`, `threads`, `sync_threads`,
`concurrency`, `preload` and `hooks`), and what a table leaves out comes
from the source's `[tool.muru]` table and then the defaults, exactly as
a deploy resolves them. A directory applies every `.toml` file in it, in
name order, as one configuration.

So a release's table needs only its `source` when the project's own
file says the rest:

```toml
# muru.daemon.toml, beside the project's pyproject.toml
[app.shop.release.r1]
source = "."
```

A file kept beside a project's code like this is named
`muru.daemon.toml`. `muru.toml` there is the project's settings file,
which has other keys, and a file in this format saved under that name is
refused as one. Anywhere else, the file's name is yours to choose.

### Writing a release's table

`muructl render` writes the release a command line describes as a file
of this form. It takes an application, a source directory and the
settings `muructl deploy` takes for a release, and prints the table:

```text
$ muructl render shop /srv/shop --release 2026-11-06 --processes 4
# The release this command line describes, in the daemon's format.
# Save it as muru.daemon.toml and give it to the daemon with:
#   muructl apply -f muru.daemon.toml
# Settings come from the flags, then the source's own settings, then the
# defaults. The source's settings are [tool.muru] in /srv/shop/pyproject.toml.
# A setting left at its default is not written; --full writes those too.

[app.shop.release.2026-11-06]
source = "/srv/shop"
entrypoint = "shop.wsgi:application"
processes = 4
threads = 8

[app.shop.release.2026-11-06.env]
DJANGO_SETTINGS_MODULE = "shop.settings.production"
```

```
muructl render shop /srv/shop --release 2026-11-06 > muru.daemon.toml
muructl apply -f muru.daemon.toml
```

The command asks nothing of a daemon and changes nothing, so it runs
where no daemon does, such as a job that writes the file for a later
step to commit. It reads the source's settings, so the source is a
directory it can read.

- **The release id** is the one `--release` names. With none the table
  is written as `r1`.
- **The source** is written as the command line gives it. A relative
  path stays relative, and `muructl apply` takes it from the file's
  directory, so `muructl render shop . > muru.daemon.toml` beside the
  code applies from there. With no source the current directory is
  written as its full path.
- **The settings** come from the flags, then the source's settings,
  then the defaults. Nothing comes from a release the application
  already has, so applying the file gives the release that `muructl
  deploy --fresh` gives for the same flags.
- **The interpreter** is written when `--python` or the source's
  settings name one. Otherwise the table has no `python` key, and the
  daemon finds the interpreter when the file is applied, as it does for
  a deploy. A release that builds its environment from `uv.lock` is
  built by the daemon when the file is applied.
- **Secrets** are written as the references `--secret` gives. No secret
  store is read.

A command line that a deploy would refuse is refused with the same
message, and nothing is printed. `--json` writes the document as JSON.

`--full` writes every setting, the defaults included, with a comment
beside each saying where it came from: the command line, the source's
settings or the default.

```text
$ muructl render shop /srv/shop --release 2026-11-06 --processes 4 --full
...
[app.shop.release.2026-11-06]
source = "/srv/shop"
entrypoint = "shop.wsgi:application"    # [tool.muru]
interface = "auto"                      # default
bytecode = "redirect"                   # default
processes = 4                           # command line
threads = 8                             # [tool.muru]
sync_threads = 8                        # default
concurrency = 1000                      # default
preload = "auto"                        # default
...

# Not set, and off unless set: timeouts.soft_timeout, timeouts.hard_timeout, memory.soft_limit, memory.hard_limit, max_requests, max_lifetime.
# python is not set: the daemon finds the interpreter when the file is applied,
# as a deploy does.
```

A file written this way keeps its meaning when a later version of Muru
changes a default.

### What an apply does

An apply creates the applications the files name, deploys each declared
release the daemon does not have (the last of an application's in id
order to serve, and the others held, unless the application's
[`serving` key](#which-release-serves) names another), creates or
updates the hosts and routes, and leaves anything already recorded as
declared alone. Id order puts ids of the form `rN` first, by number, so
`r10` comes after `r9`, and every other id after them, by name: files
adding `r9` and `2026-11-06` serve `2026-11-06`. Releases are
immutable: a declared release whose settings differ from the recorded
ones is an error, and the file is applied only when it has no errors at
all. Unknown keys, invalid names, a missing source directory and a
source without an entrypoint are errors too. `[[listener]]` entries and
the `[limits]` table belong in the daemon's own configuration file; in
an applied file they are reported as warnings and not applied.

Four flags say how the releases an apply adds are deployed, as they do
on `muructl deploy`:

| Flag | What it does |
| --- | --- |
| `--hold` | Starts the releases and leaves traffic where it is |
| `--no-wait` | Returns once the releases are recorded |
| `--retain DURATION` | How long a release the apply replaces stays warm |
| `--retain-workers N` | The workers each group of that release keeps meanwhile |

```
muructl apply -f muru.daemon.toml --hold
muructl rollout bluegreen shop@2026-11-06
```

A held release takes no traffic and is reached with the `Muru-Preview`
header, until a [rollout](rollouts.md) or a promotion moves traffic to
it. `--hold` holds every release the apply adds, in every application,
and the plan and the report say `held` of each:

```console
$ muructl apply -f config/ --hold --dry-run
muru: plan (nothing changed)
  deploy          api@r8                   held: api.asgi:application 2x8 from /srv/api
  deploy          shop@2026-11-06          held: shop.wsgi:application 4x8 from /srv/shop
```

An application whose first release is held serves nothing until a
rollout. To hold the releases of one application and not another's,
give it a `serving` key naming the release that serves now, which holds
the others without the flag.

`--dry-run` prints the plan and changes nothing. For each release that
would take over from one that serves, it says how long that one stays
warm. `--prune` also removes the routes, hosts, releases and
applications the files do not contain, in that order: a running release
drains and stops first. The release that serves each application after
the apply must be declared, or the apply is refused, since `--prune`
would remove it, and so must every release a route the files keep names
in its traffic, and every application such a route sends requests to. `--json` writes the report as JSON. The exit code is 0
when there is nothing to do or everything applied, 2 for a dry run with
changes planned, and 1 for errors, for a change that failed, and for a
snapshot the edge did not take, which the report names: the changes
were made, and the edge serves what it had until a later snapshot
reaches it.

`muructl config export` writes desired state as a file of this form, with
a header naming the daemon's version, the node, the time and the state
version. It writes the release that serves each application, every key
spelled out, and the application's `serving` key naming it, and the
releases its routes' splits and client classes name, so applying the
file back to the daemon it came from changes nothing and, applied to
another node, it starts those releases held. A release neither is left
out. `--app NAME` writes one application, and `--json` the same document
as JSON. A [rollout](rollouts.md) under way is not part of the file;
with `--resolve-rollouts` it is written as where it stands, the
candidate declared beside the serving release and the application's
routes splitting traffic between them at the candidate's current share.
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

### Which release serves

An application's `serving` key names the release that serves its
traffic. Declaring a new release beside it stages that release:

```toml
[app.shop]
serving = "r1"

[app.shop.release.r1]
source = "/srv/shop/r1"

[app.shop.release.r2]
source = "/srv/shop/r2"
```

Applied while r1 serves, this starts r2 held. It does not take the
application's traffic: it serves only the share a route's split gives
it, and requests that name it with the `Muru-Preview` header, so a split
of 0, or none, sends it nothing else.
Changing the key to `serving = "r2"` and applying the files again
promotes r2: all of the application's traffic moves to it, and r1
drains and stays warm as `--retain` and `--retain-workers` say. The
promotion comes after the apply has written the hosts and routes, so a
split in the same files that still names r1 keeps it running. The plan
says what happens to each release:

```console
$ muructl apply -f config/ --dry-run
muru: plan (nothing changed)
  promote         shop@r2                  serving in place of shop@r1
  2 objects as declared
muru: shop@r1 stays warm for 1800s with 1 worker to a group, when shop@r2 takes its place
```

The key names a declared release or one the daemon already has; naming
neither is an error. Without the key, the last release the apply adds,
in id order, serves, and the others it adds are held. `--hold` with a
key that would move traffic is an error, since the flag leaves traffic
where it is.

While a [rollout](rollouts.md) is under way, the rollout moves the
traffic. The key may name its baseline or its candidate, and the apply
promotes nothing. Naming the baseline also warns that once the rollout
promotes the candidate, applying the same files again promotes the
baseline back: change the key to the candidate before the rollout ends
to keep its outcome. Naming any other release is an error until the
rollout ends, or `muructl rollout promote` or `muructl rollout abort`
ends it. Once a rollout has rolled back, by its gates or by an abort,
naming its candidate is an error too, since the apply would promote it
past its gates: name another release, or promote it by hand with
`muructl rollout bluegreen`, after which the files and the daemon
agree.

## Building the environment on the node

A source with a `uv.lock` and no `.venv` gets an environment of its own,
built by the daemon before the release starts: `uv sync --frozen` without
the dev group, without building sdists, with bytecode compiled and the
project's extras and groups, into the state directory under
`envs/APP/RELEASE`, as the release's user. The release is in the
`building` state meanwhile, uv's lines go to the release log tagged
`[build]`, and a failed build fails the release with uv's last line. A
source with its own `.venv` is used as it is, as before.

```
muructl deploy shop /srv/shop                        # builds when the source has a lock and no .venv
muructl deploy shop /srv/shop --build always          # builds although the source has a .venv
muructl deploy shop /srv/shop --build never           # runs on what is there
muructl deploy shop /srv/shop --build-extra postgres
```

The same keys live in the project's `[tool.muru.build]` table and in a
declared release's `build` table: `when`, `extras`, `groups`,
`source_builds` and `compile_bytecode`. `muructl release show` names the
environment a release runs on, and it is removed with the release. A
Python the request names and the node lacks is downloaded into the state
directory first, unless the `[tools]` table says `python_downloads =
"never"` or `offline = true`.

## Bytecode caches

Python writes compiled bytecode into `__pycache__` beside the source,
which an immutable release directory should not take and a release
directory owned by another user cannot. Under the daemon, bytecode goes to
a directory of the release's own under the state directory, `pycache/APP/RELEASE`,
writable by the release's user and removed with the release, so cold
starts stay fast and the source tree stays clean. A source that already
holds bytecode for the release's interpreter, compiled by `muructl build
--deployment` or `python -m compileall`, is used as it is. `bytecode` in
the project's `[tool.muru]` table, in a declared release or as
`--bytecode` on `muructl deploy` takes `redirect` (the default), `source`
or `off`, and `muructl release show` says which is in force.

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
`MURU_CACHE_DIR` and `MURU_DAEMON_CONFIG`, and the directories systemd
provides through `StateDirectory=` and its siblings are used when set.
`muructl daemon run` prints every location and which rule chose it.

The daemon's own commands take all five flags: the `muructl daemon`
commands, `muructl up` and `muructl mcp serve --up`, so one set of
flags starts, stops and reloads a daemon. So does `muructl secret`,
which reads the daemon's file. Every other command only talks to a
running daemon, and takes `--socket`, `--run-dir` and `--system` to find
it. The run
directory must be an absolute path, and a Unix socket path is limited to
104 bytes on macOS and 108 on Linux, so a run directory too deep to leave
room for the workers' sockets, which sit about 35 bytes plus the
application's name below it, is refused at start with a message saying
so.

The configuration file is optional. Its `[[listener]]` entries name the
addresses to serve, each with `tls = true` when the edge terminates TLS
on it, and its `[limits]` table sets the body size limit and timeouts.

## Running it

`muructl daemon start` starts the daemon in the background, logging to
`daemon.log` in the log directory, and waits until it answers. The
daemon writes its pid to `daemon.pid` in the runtime directory and
removes the file when it exits. `muructl
up` does the same and is harmless when one already runs. `muructl daemon
status` and `muructl status` show the daemon, its edge process, its
listener and its locations. `--metrics ADDRESS`, or a `[metrics]` table
in the configuration file, serves Prometheus metrics on an internal
address; the [metrics guide](metrics.md) lists them. `--dashboard
ADDRESS`, or a `[dashboard]` table, serves the [dashboard and JSON
gateway](dashboard.md) on a loopback address, for a session signed in
with `muructl dashboard login`. An `[overlay_webhook]`
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

The access log is written as JSON lines to `access.log` in the log
directory, `muructl logs` shows or follows it and each release's output,
and `muructl logs reopen` works with logrotate; the [logging
guide](logging.md) has the details. The events `muructl events` shows are
kept in the state store, and `[[events.sink]]` entries deliver them to
webhooks; the [events guide](events.md) has the details.

## Stopping and restarting

`muructl daemon stop` detaches: the daemon exits, and the edge and every
host keep serving exactly as they were. The next `muructl daemon start`
(or `up`, or a service manager restarting the unit) finds them through
the records in the runtime directory, takes them over, and carries on,
so a daemon restart or upgrade never costs a request. `muructl daemon
stop --shutdown` drains every release, stops the hosts and the edge, and
then exits, for taking a node out of service. It leaves no records
behind in the runtime directory, and the next daemon starts the current
releases afresh.

A host whose daemon is gone keeps its workers; a host that dies takes
its workers with it, and the next daemon starts its release again. A
worker that is killed, by a hard timeout say, may leave a subprocess a
request started; on Linux the host collects it, giving it the release's
shutdown timeout to finish before it is stopped, so nothing lingers or
sits as a zombie.

The shutdown is one sequence, and it fits a budget. The daemon file's
`[shutdown]` table sets the parts:

```toml
[shutdown]
on_sigterm = 'detach'   # or 'shutdown'
delay = '0s'
budget = '8s'
```

For `delay` the edge keeps accepting and serving, with every response
carrying `Connection: close` (GOAWAY on HTTP/2) and idle connections
closed, so clients with keep-alive connections reconnect, to another
node when a load balancer is in front; the default of none suits a
single node. Then the edge stops accepting and the requests in flight
finish, every host stops at once, each given its workers' stop budget or
what remains of the shutdown's, whichever ends first, and the edge goes
last, so the workers' WebSocket close frames reach their clients. The
whole takes at most `budget`; whatever has not finished by then is
killed. A long `shutdown_timeout` on a release wants a budget to match.

SIGTERM (and an interrupt) means `muructl daemon stop` without
`--shutdown`, that is a detach, which is what a service manager's restart
wants, unless `on_sigterm = 'shutdown'` makes it the shutdown above. In a
container that is the default; see below.

`muructl daemon run` runs it in the foreground, for a service manager or
a terminal; SIGTERM or an interrupt detaches it the same way, and
`muructl daemon stop --timeout` bounds how long the command waits for the
daemon to exit. The [systemd
guide](systemd.md) has the unit and what it does. Commands find the
daemon through its admin socket: `--socket` or `MURU_SOCKET`, else a
development run started from the current directory, else the daemon's
socket in the runtime directory, else the one development run listening;
with several runs and no daemon the command names them, so one can be
chosen with `--socket`. `muructl status` says which was found.

## In a container

`muructl daemon run` can be a container's main process. When muructl
finds itself PID 1 it starts as an init of its own, as `tini` would: PID
1 forwards SIGTERM, SIGINT, SIGHUP, SIGUSR1, SIGUSR2 and SIGQUIT to the
daemon, which runs as its child, collects every process that exits, so
nothing in the container stays a zombie, and exits with the daemon's
status when the daemon exits. An image that uses `tini` or `docker run
--init` sets `MURU_NO_INIT=1`, and the daemon runs as PID 1 itself.

Under the init, and for a daemon that is PID 1 itself, SIGTERM means
shutdown rather than detach: when PID 1 exits the kernel kills every
other process in the container, so detaching would kill the edge and
every worker mid-request. `docker stop` therefore drains and stops
everything inside the budget above, whose default of eight seconds is
under the ten `docker stop` allows, and the container exits 0. A
`[shutdown]` table with `on_sigterm = 'detach'` turns that off, for a
container that keeps its hosts across a daemon restart of its own.

The daemon listens on `127.0.0.1:8080` unless told otherwise, so in a
container it takes `--listen 0.0.0.0:8080` or a `[[listener]]` entry,
and its applications and logs are in the locations above, so a volume
for the state store keeps the deployed releases across a restart of the
container. To run one application in a container, with its configuration
from the image and its logs on standard output, use `muructl serve`
instead; [in a container](containers.md) covers it.

The daemon does not know it is in a container, and does not need to: it
resolves interpreters and builds environments as it does on a node, which
is right in a development container, where the source is mounted and the
network is there. An image built ahead of time, with its interpreters and
environments in it, says so in the daemon file, and the daemon then
installs nothing at run time and fails at once, naming what the image
lacks:

```toml
[tools]
offline = true                  # nothing is downloaded, the runtime library included
python_downloads = "never"      # a Python the image lacks fails the release
```

with `--build never` on each deploy, or `when = "never"` in the project's
`[tool.muru.build]` table or a declared release's `build` table, so that
the environment in the image is used as it is.

## Upgrading

Installing a new version of Muru changes nothing until `muructl daemon
upgrade`, which re-executes the daemon from the installed `muructl`,
keeping its process id and its children, and replaces the edge without
refusing a connection. Releases keep the runtime library they started
with until a new release replaces them. [Upgrading Muru](upgrading.md)
covers the command, development runs, the daemon and releases.

## Reloading the configuration

`muructl daemon reload`, or SIGHUP, reads the daemon's configuration
file again, checks it whole, and applies what changed:

- the `[proxy]`, `[compression]`, `[error_pages]`, `[error_override]`,
  `[[client_class]]`, `[rate_limit]` and `[limits]` tables, and the
  `h2c` setting and the `limits` and `proxy` tables of a `[[listener]]`
  entry, which the edge is given at once; a connection already open keeps the limits it was accepted
  under;

- the `[[listener]]` entries: a changed set starts a new edge on the new
  listeners and tells the old one to stop, as `muructl daemon upgrade`
  does, so a listener that stays keeps its socket and its backlog and no
  request is lost; a listener that cannot be bound (its port in use,
  say) fails the reload before anything is applied;

- the `[[events.sink]]` entries: a sink added starts, one dropped stops,
  and one whose entry changed is restarted; a sink's cursor stays in the
  state store, so one that comes back delivers what it missed;

- the certificate and key files of every applied TLS host, read again
  and, when changed, sent to the edge; the [virtual hosts
  guide](virtual-hosts.md#tls) has the detail;

- the `[secrets.store.NAME]` tables, which the next release to start
  resolves its secrets from;

- the `[logs]` table: the daemon rotates on the new policy from its
  next check, and a period that changed rotates each file once; the
  access log's `access_log`, `access_log_exclude` and `slow` go to the
  edge at once, for the requests completed from then on;

- the `[shutdown]` table, for the next stop signal or `muructl daemon
  stop --shutdown`;

- the [node's budget](scaling.md#the-nodes-budget), `[daemon.budget]`;

- the dashboard's `actions` switch, `[dashboard] actions`.

The rest of the file is read when the daemon starts: the `[metrics]`
table, the `[dashboard]` table's `listen` address, the
`[overlay_webhook]` table and the `[daemon]` table's `user`. A change to
those takes effect after `muructl daemon stop` and `muructl daemon
start` (`systemctl restart muru` under systemd), which detaches from the
hosts and the edge and takes them back, so it costs no request. A daemon
started with `--listen`, `--metrics`, `--dashboard` or `--user` keeps
those over the file's settings on every reload.

The command prints what changed and what was done about it, what is
unchanged and what is read at start, and the `config.reloaded` event
records the same. A file the daemon cannot parse, or a table it refuses,
leaves everything as it was and the command fails saying why.
