`muructl run` now defaults to one process, reports `wsgi.multiprocess` as
False for a single-process run, accepts request bodies up to 100 MB
(`--max-body-size` changes it, and `muru.max_body_size` in the environ
shows the limit in force) and warns once when the application sends a
Strict-Transport-Security header, which browsers would apply to every
server on localhost.
