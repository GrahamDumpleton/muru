Added built-in log rotation: the daemon rotates the access log, the
edge's and each release's log and its own daily or at a size, compresses
the archives and keeps the newest, from the `[logs]` table; `rotate =
'external'` leaves rotation to logrotate.
