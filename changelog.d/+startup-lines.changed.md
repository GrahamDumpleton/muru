`muructl run` now prints a line for each worker group and each mount when it
starts, the host says what each mount's entrypoint is served as (also when
the workers import the application themselves), the serving line names the
event loop a WSGI release has beside its threads when a mount is ASGI, and
counts of one are singular: "1 worker of 4 threads".
