`muructl run` now stops in order on SIGTERM as it does on Ctrl-C, and a host
whose parent has gone away still stops its workers; before, both could
leave worker processes running.
