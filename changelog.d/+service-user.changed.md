Changed a daemon that runs as root to start the edge, and every
application that names no `user`, as a service user (`muru`, or the one
`--user` or the `[daemon]` table names) and never as root; it refuses to
start when the account does not exist, and `user = 'root'` on an
application is recorded as an `app.user_is_root` warning each time a
release starts.
