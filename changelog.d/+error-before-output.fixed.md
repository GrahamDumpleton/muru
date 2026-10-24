An application that fails after starting its response but before any of it
has reached the edge now gets the 500 error page, with the traceback in
development mode, instead of a dropped connection.
