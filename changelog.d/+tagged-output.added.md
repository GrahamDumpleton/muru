Each worker's stdout and stderr are pipes the host relays to the terminal, every line tagged with the worker's index (and the stream for stdout), so output from different workers can be told apart.
