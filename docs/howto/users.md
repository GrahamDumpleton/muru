# Per-application users

```{admonition} Status
:class: note

A root daemon runs the edge and every application as a service user or
as the user an application names, never as root unless told to. Resource
limits through cgroups, the admin group and dropping the daemon's own
capabilities are still to come.
```

## The service user

A daemon that runs as root (`muructl daemon run --system` under systemd,
say) keeps root for itself alone: it binds the listeners, creates the
run directories and switches user for its children. Everything else runs
as the service user: the edge, which parses client traffic, and every
application that names no user of its own. The service user is `muru`
unless `--user NAME` or the `[daemon]` table says otherwise:

```toml
[daemon]
user = 'www-data'
```

Muru never creates the account. A root daemon whose service user does
not exist, or is root, does not start, and says what to do: create the
account, name another, or run the daemon as that user instead. The
system packages create `muru` through sysusers.d; by hand, one line
does it:

```
$ sudo useradd --system --no-create-home --shell /usr/sbin/nologin muru
```

The service user must be able to read what it serves: each application's
source tree and interpreter, and the directories static routes serve.
Certificates and keys are read by the daemon and passed on, so they can
stay root's.

## A user per application

Each application can name a user of its own, so applications never see
each other's files and a compromised one has only its own user's rights:

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
workers inherit. A user or group that does not exist fails the release.

The release's socket directory belongs to the application's user with
the service user's group, and opens to those two alone, so the edge
reaches the workers' sockets and nothing else does. The admin socket and
the daemon's own files stay the daemon's.

`user = 'root'` is allowed, since it is explicit, and is not recommended:
a compromised application would have the node. Each release of such an
application that starts records an `app.user_is_root` warning event.

## Without root

A daemon that is not root runs everything as its own user. An
application that names a user still starts, and the daemon records an
`app.user_not_applied` warning event for each release it starts, so the
difference between a laptop and a server is visible rather than silent.
A service user named with `--user` or `[daemon]` is noted in the
daemon's log as not applied, for the same reason.
