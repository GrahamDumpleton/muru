The edge binds its HTTPS listener together with the HTTP one, before it connects to the workers, so a client that finds one port open during start-up finds both.
