# From gunicorn to Muru

```{admonition} Status
:class: note

This tutorial assumes you have done
[deploy with the daemon and roll back](deploy-and-roll-back.md) and
[your first canary](first-canary.md). The same steps work for uvicorn,
Granian and mod_wsgi-express, each with a profile of its own.
```

An application already in production under gunicorn does not have to
switch servers in one go. Muru can run the gunicorn command you have
today as a release of its own, serve the application's traffic through
it, and then move that traffic to Muru's own workers a share at a time,
judged as a canary, with the gunicorn release kept warm to go back to.
Nothing about the application changes along the way.

In about fifteen minutes you will run a small Flask application under
gunicorn, hand that gunicorn command to the daemon, start the same
application on Muru's workers beside it, canary from one to the other
with requests running, and retire gunicorn without a request failing.

## The deployment you have

Make a project with Flask and gunicorn; Muru's runtime library is
provided by `muructl` itself:

```
mkdir legacy && cd legacy
uv init --bare
uv add flask gunicorn
```

Save this as `legacy.py`:

```{literalinclude} code/legacy/legacy.py
:language: python
```

The page says which server answered it: gunicorn puts its name and
version in `SERVER_SOFTWARE`, and Muru's workers add keys of their own to
the WSGI environ, `muru.request_id` among them. This is how it runs
today:

```
uv run gunicorn --workers 2 legacy:app
```

```
curl http://127.0.0.1:8000/
```

```
Hello from gunicorn/26.2.0
```

Stop it with Ctrl-C.

## Hand gunicorn to the daemon

Start the daemon if it is not running, and deploy the application as a
managed HTTP server, with the gunicorn command line after `--`:

```
muructl daemon start
muructl deploy legacy . --server gunicorn -- .venv/bin/gunicorn --workers 2 --bind '{bind}' legacy:app
```

```
muru: release legacy@r1 is ready and serving
```

`--server gunicorn` picks the profile that knows how gunicorn binds and
how it is stopped. The daemon replaced `{bind}` with a Unix socket of the
release's own, started gunicorn in the source directory, waited until it
answered, and routed the application's requests to it through the edge
on port 8080:

```
curl http://127.0.0.1:8080/
```

```
Hello from gunicorn/26.2.0
```

`muructl release list legacy` shows the release with `1 gunicorn` where a
release of Muru's workers shows its processes and threads, and
`muructl logs legacy@r1` shows gunicorn's own log. From here on gunicorn
is supervised: an instance that exits is started again, and the release
can be drained, kept warm and rolled back to like any other.

## The same application on Muru's workers

Deploy the same directory again, this time naming the WSGI callable, and
hold it out of traffic:

```
muructl deploy legacy . --entrypoint legacy:app --hold
```

```
muru: release legacy@r2 is ready and held: not serving
```

`r2` runs `legacy.py` unchanged, in the project's virtualenv, on Muru's
workers. A `[tool.muru]` table in `pyproject.toml` with
`entrypoint = "legacy:app"` would save naming it on each deploy.

## Cut over

Start a loop of requests in a second terminal and leave it running:

```
while true; do curl -s http://127.0.0.1:8080/; done
```

Then canary from gunicorn to Muru's workers, holding each step for 30
seconds:

```
muructl rollout canary legacy@r2 --steps 10,50 --hold 30s
```

`r2` takes 10% of the requests, judged against gunicorn on its error
rate over the same seconds, and the loop starts to show
`Hello from Muru` among the `Hello from gunicorn/26.2.0` lines. While it
runs, either release can be asked for by name:

```
curl -H 'Muru-Preview: r2' http://127.0.0.1:8080/
curl -H 'Muru-Preview: r1' http://127.0.0.1:8080/
```

Follow it to the end:

```
muructl rollout status legacy --wait
```

```
muru: rollout 1 running step 1 of 2: 10% for 30s
muru: rollout 1 running step 2 of 2: 50% for 30s
muru: rollout 1 promoted: every step passed its gates
```

Every line of the loop now says `Hello from Muru`, and none of them was
an error: the requests in flight on gunicorn when each share moved
finished there. gunicorn is still running, kept warm, so
`muructl rollback legacy` would hand the traffic straight back to it.

## Retire gunicorn

When you are sure, drain it:

```
muructl drain legacy@r1
```

```
muru: release legacy@r1 is stopped
```

The daemon stopped taking requests for it, waited for those in flight,
and sent gunicorn the signal its profile names, after which gunicorn
shut its workers down as it always does; its log ends with
`Shutting down: Master`. Stop the loop with Ctrl-C.

## From here

- [Managed HTTP servers](../howto/managed-servers.md) covers the profiles,
  binding to ports, several instances, readiness probes and declaring a
  managed release in a file.

- [Flask and Django](../howto/flask-and-django.md) has what to know
  about running each framework on Muru's workers.

- [Rollouts](../howto/rollouts.md) has the other gates a cutover can be
  judged by, such as the 95th percentile request time.
