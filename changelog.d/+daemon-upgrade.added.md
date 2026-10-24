Added `muructl daemon upgrade`, which re-executes the daemon from the
installed binary without interrupting traffic: the daemon keeps its pid,
takes its hosts and edge over again, and replaces the edge with one from
the new version, the old edge finishing its connections first.
