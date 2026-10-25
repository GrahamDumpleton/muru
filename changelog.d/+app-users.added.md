Added per-application users: `[app.NAME]` takes `user`, `group` and
`umask`, which a daemon running as root applies to the application's
hosts and workers, and a daemon that is not root reports as not applied.
