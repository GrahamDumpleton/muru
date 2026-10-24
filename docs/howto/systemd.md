# Running under systemd

```{admonition} Status
:class: note

The shipped unit runs the daemon in system mode with readiness
notification, detaches on stop and re-reads the `[proxy]` table on
reload. Reloading listeners, limits and event sinks, journald as the log
sink and socket activation are still to come.
```

The repository ships a unit at `packaging/systemd/muru.service`:

```
[Unit]
Description=Muru
After=network-online.target

[Service]
Type=notify
ExecStart=/usr/bin/muructl daemon run --system
ExecReload=/usr/bin/muructl daemon reload
ExecStop=/usr/bin/muructl daemon stop --shutdown --timeout 60s
TimeoutStopSec=90
ConfigurationDirectory=muru
StateDirectory=muru
CacheDirectory=muru
LogsDirectory=muru
RuntimeDirectory=muru
RuntimeDirectoryPreserve=yes
KillMode=process
Restart=on-failure
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
```

Install it, put the configuration file at `/etc/muru/muru.toml`, and
start it:

```
$ sudo cp packaging/systemd/muru.service /etc/systemd/system/
$ sudo systemctl daemon-reload
$ sudo systemctl enable --now muru
$ muructl --system status
```

## What the unit does

The directory settings make systemd create `/etc/muru`, `/var/lib/muru`,
`/var/cache/muru`, `/var/log/muru` and `/run/muru` and pass them to the
daemon in the environment, so the daemon finds its locations without any
path being written twice; `muructl daemon run` prints each one and the
rule that chose it. `--system` makes commands run as other users look in
the same places.

`Type=notify` means systemd waits for the daemon to say it is ready,
which it does once the admin socket answers and the releases that were
serving have been taken over, and `systemctl start` returns only then.
The status line systemd shows names the listeners.

`systemctl stop` runs the `ExecStop` line: `muructl daemon stop
--shutdown` drains every release, stops the hosts and the edge, then the
daemon exits, which is what a node going out of service wants.
`--timeout` bounds how long muructl waits for that, and `TimeoutStopSec`
is longer, so a drain that overruns is reported before systemd kills what
is left.

`systemctl restart`, or a daemon that is killed or crashes, is different:
`KillMode=process` means systemd stops only the daemon, and the daemon
treats SIGTERM as `muructl daemon stop` without `--shutdown`, that is it
detaches, leaving the hosts and the edge serving. `RuntimeDirectoryPreserve`
keeps their sockets in `/run/muru`, and the next daemon finds them there
and carries on, so a restart or an upgrade of the daemon costs no
request. The [daemon guide](daemon.md) says more about detaching and
re-adoption.

`systemctl reload` runs `muructl daemon reload`, which re-reads the
configuration file for what can change while the daemon runs: today the
`[proxy]` table, which the edge is given at once. Listeners, limits, the
metrics listener and event sinks are read when the daemon starts, and the
reload says so. SIGHUP does the same as the command.

## Logs

The daemon's own output goes to the journal, where `journalctl -u muru`
shows it. Each release's host writes to `/var/log/muru/apps/APP/RELEASE.log`
and the edge to `/var/log/muru/access.log` and `edge.log`, which `muructl
logs` shows or follows and `packaging/logrotate/muru` rotates; the
[logging guide](logging.md) has the details.
