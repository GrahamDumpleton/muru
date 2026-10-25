Fixed `muructl run` and `muructl serve-static` being killed by a Ctrl-C or
SIGTERM that arrived while the workers were still starting; the run now stops
in order, the workers running their drain and exit hooks, whenever the signal
comes.
