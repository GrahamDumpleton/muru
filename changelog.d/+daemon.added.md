The daemon: `muructl daemon start` (or `muructl up`) runs a daemon with
its own state store, runtime and log directories, and `muructl deploy APP
SOURCE` creates a release from a source directory, starts it, moves
traffic to it when every worker is ready and drains the release that
served before, so no request is refused during a deploy; the replaced
release stays warm for 30 minutes (`--retain`) so a rollback within that
window is instant. `muructl release
list` and `wait`, `rollback`, `drain`, `events` and `status` manage and
watch it; a release whose host dies is started again. The edge runs as a
process of its own on a socket the daemon binds and hands to it, and is
started again on the same socket if it dies. `daemon stop` detaches,
leaving the edge and the hosts serving for the next daemon to take over,
so a daemon restart costs no request; `daemon stop --shutdown` drains
and stops everything.
