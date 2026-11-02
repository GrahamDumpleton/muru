# Upgrading Muru

Installing a new version of Muru changes nothing that is already running.
A development run keeps the version it started with until it is started
again, the daemon until `muructl daemon upgrade`, and each release the
runtime library it started with until a new release replaces it. Each
step can be taken when it suits, and none of them costs a request.

## Before upgrading

Read the [changelog](../project/changelog.md) for the versions between
the one installed and the new one. Until 1.0, a version can change
configuration keys, command options and the shape of `--json` output,
and the changelog lists every such change. `muructl --version` prints
the installed version.

## The command

Upgrade `muructl` the way it was installed:

```
uv tool upgrade muructl
pipx upgrade muructl
pip install --upgrade muructl    # in the environment it was installed into
```

On a node, install the new version over the old at the path the service
runs, `/usr/bin/muructl` in the [systemd unit](systemd.md), with the
package manager or by copying the binary.

## Development runs

A `muructl run` already running is the old version. Stop it and start it
again. The first run of the new version on each Python version fetches
the matching runtime library once, into your user cache, as the first run
of any version does (see [the runtime
library](interpreters.md#the-runtime-library)); with `MURU_OFFLINE=1`,
put the new version's wheels in the directory `MURU_WHEEL_DIR` names
first.

If the project lists `muru` as a development dependency for editor
completion, update it to the new version too, so the copy in the project
matches and is used.

## The daemon

```console
$ muructl daemon upgrade
muru: daemon (pid 4120, version 1.0.0) re-executing from /usr/bin/muructl (version 1.1.0)
muru: daemon upgraded to version 1.1.0 (pid 4120); edge pid 4133 replaced by pid 5871
```

The daemon re-executes itself from the installed `muructl`, the one its
command name finds on the `PATH` or the path it was started with,
keeping its process id and its children, and the new version takes the
hosts and the edge over from their records, exactly as a restarted daemon
does. It then starts a new edge from its own version on the same
listening sockets, gives it the routing in force, and tells the old edge
to stop accepting: that edge finishes the requests in flight, closes its
idle keep-alive connections (with GOAWAY on HTTP/2) and exits. No
connection is refused and no request fails in between; WebSocket
connections still open on the old edge are closed when it exits.

The command waits until the upgraded daemon answers with its new edge.
The daemon checks that the installed binary runs before it commits, and
the `daemon.upgraded` and `edge.replaced` events record what happened.
`muructl status` shows the daemon's version afterwards.

A system daemon is upgraded the same way:

```
muructl --system daemon upgrade
```

Under systemd the service's main process stays the same through an
upgrade, so the unit is not restarted. `systemctl restart` is not the
way to upgrade: its stop drains and stops every release, as taking a
node out of service does.

## Releases

A release keeps the runtime library it started with, through a daemon
upgrade and for as long as it runs. A release created after the upgrade,
by a deploy, an apply or a rollout, starts with the new version's
library. To move an application onto it, deploy the same source again:

```
muructl deploy shop /srv/shop
```

The new release takes its settings from the one before it, as any deploy
does, and the old release drains and is retained, so `muructl rollback
shop` goes back to the old library too. A rollout moves the application
across in stages instead (see [rollouts](rollouts.md)).

A daemon with `offline = true` in its `[tools]` table fetches nothing:
put the new version's wheels in its `wheel_dir` before the first deploy
after the upgrade.

## Coding agents

A project that carries the [coding agent skill](coding-agents.md#the-skill)
has a copy written for the old version. Run `muructl mcp skill install`
in the project and commit the result; `muructl mcp skill status` says
whether it is due.
