# Per-application users

```{admonition} Status
:class: note

An application's user, group and umask, applied by a daemon that runs as
root and reported as not applied by one that does not. Resource limits
through cgroups and the admin group are still to come.
```

Under a daemon that runs as root (`muructl daemon run --system` under
systemd, say), each application's hosts and workers can run as a user of
their own, so applications never see each other's files and a
compromised one has only its own user's rights:

```toml
[app.shop]
user = 'shop'
group = 'shop'          # the user's primary group when left out
umask = '027'           # octal; the daemon's own when left out
```

The settings go in the application's table for `muructl apply`, are
exported with it, and `muructl app list` shows them. Changing them
takes effect on the next release that starts: the daemon looks the user
and group up, gives the release's run directory, its secret files and
its log to that user, and starts the host with that identity, which the
workers inherit. The daemon, the edge and the admin socket keep the
daemon's own user, and the edge reaches the workers' sockets as root.

A daemon that is not root runs everything as its own user. An
application that names a user still starts, and the daemon records an
`app.user_not_applied` warning event for each release it starts, so the
difference between a laptop and a server is visible rather than silent.
A user or group that does not exist fails the release.
