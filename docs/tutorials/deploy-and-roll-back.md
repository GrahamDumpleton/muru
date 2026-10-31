# Deploy with the daemon and roll back

```{admonition} Status
:class: note

This tutorial runs the daemon as your own user, with its files in your
user directories. It assumes you have done
[the first tutorial](first-wsgi-app.md).
```

`muructl run` is for development: it serves one application until you
stop it. The daemon is for an application that stays up. It runs in the
background, keeps every release you deploy in a state store, and moves
traffic from one release to the next without refusing a request.

In about fifteen minutes you will start the daemon, deploy a small
application, deploy a release that fails to start and see the one before
it keep serving, deploy a fix, roll it back, and stop and start the daemon
without the application noticing.

## A project with releases

A release is a source directory. Here each release is a copy of the
application in a directory of its own, `v1`, `v2` and so on, all sharing
the project's virtualenv:

```
mkdir site && cd site
uv init --bare
uv venv
mkdir v1
```

Save this as `v1/app.py`:

```{literalinclude} code/releases/v1/app.py
:language: python
```

`muru.context()` tells the application where it runs, so each response
says which release answered it.

## Start the daemon

```
muructl daemon start
```

```
muru: daemon started (pid 77672), serving on http://127.0.0.1:8080, log /Users/you/Library/Logs/muru/daemon.log
```

The daemon is running in the background now, with an edge process that
listens on port 8080. Nothing is deployed, so the edge answers every
request with 503 and the reason `no_workers`:

```
curl http://127.0.0.1:8080/
```

`muructl status` shows the daemon, its edge, where it keeps its state
and what is deployed.

## Deploy the first release

```
muructl deploy hello v1 --entrypoint app:application --python .venv/bin/python
```

```
muru: release hello@r1 is ready and serving
```

That created the application `hello` and its first release, `r1`, started
the release's host and worker, waited for the worker to be ready, and
moved traffic to it:

```
curl http://127.0.0.1:8080/
```

```
Hello from hello@r1
```

The entrypoint and the interpreter are recorded with the release, and the
next deploy of `hello` takes them from it, so they are needed only the
first time. A project can also name them in its `[tool.muru]` table.

`muructl release list hello` shows the release, and `muructl events` the
story so far:

```
     2 09:53:52 info     release.created      hello@r1                 from /Users/you/site/v1
     3 09:53:52 info     release.ready        hello@r1                 1 workers ready in 0.1 s
     4 09:53:52 notice   traffic.switched     hello@r1                 traffic moved to the release
```

## A release that fails

Make the second release, which reads its greeting from an environment
variable:

```
cp -R v1 v2
```

and change `v2/app.py` to this:

```{literalinclude} code/releases/v2/app.py
:language: python
:emphasize-lines: 1,5
```

Nothing sets `GREETING`, so the import fails. Deploy it:

```
muructl deploy hello v2
```

```
Error: the host exited with exit status: 1 before its workers were ready
```

The new release never took traffic. `curl` still gets
`Hello from hello@r1`, and `muructl release list hello` shows `r2` as
failed beside `r1`, which still serves. The release's own log says what
went wrong:

```
muructl logs hello@r2
```

```
muru.host[77760]: cannot load app:application: KeyError: 'GREETING'
```

A deploy only moves traffic once every worker of the new release is
ready, so a release that cannot import, or whose workers fail as they
start, costs nothing but the attempt.

## Deploy a fix

Make a third release from the first, with a new greeting:

```
cp -R v1 v3
```

```{literalinclude} code/releases/v3/app.py
:language: python
:emphasize-lines: 3
```

To see that the switch refuses nothing, keep requests going in another
terminal while you deploy:

```
while true; do curl -s http://127.0.0.1:8080/; sleep 0.1; done
```

```
muructl deploy hello v3
```

The loop's lines change from `Hello from hello@r1` to
`Hello again from hello@r3` with no error in between. The new release
started beside the old one, traffic moved once its worker was ready, and
the old one finished the requests it had and stopped taking more.

`muructl release list hello` now shows `r3` serving and `r1` retained:

```
RELEASE                  STATE      SERVING  WORKERS    SOURCE
hello@r3                 ready      yes      1x4        /Users/you/site/v3
hello@r2                 failed              1x4        /Users/you/site/v2
                         the host exited with exit status: 1 before its workers were ready
hello@r1                 retained            1x4        /Users/you/site/v1
                         warm until 10:24 UTC; a rollback to it is instant
```

## Roll back

Suppose `r3` turns out to be wrong. Roll back:

```
muructl rollback hello
```

```
muru: rolled back to hello@r1 (ready)
```

The loop is back to `Hello from hello@r1` at once. A release that is
replaced is kept warm for 30 minutes with its host and a worker still
running, so moving traffic back to it needs nothing to start. `r3` is
now the retained one, and `muructl rollback hello` again would take you
forward to it. The events have every step, `app.rolled-back` among them.

## The configuration as a file

Everything the daemon was told is in its state store, and it can write
it out as a file:

```
muructl config export
```

```toml
[app.hello.release.r1]
source = "/Users/you/site/v1"
entrypoint = "app:application"
interface = "auto"
python = "/Users/you/site/.venv/bin/python"
processes = 1
threads = 4
sync_threads = 8
concurrency = 1000
preload = "auto"
```

Kept in version control, a file like this can be applied to a daemon
with `muructl apply -f`, which deploys what it declares and leaves what
already matches alone.

## Stop and start the daemon

```
muructl daemon stop
```

```
muru: daemon detached; hosts and the edge keep serving
```

The loop does not notice. The daemon manages the processes that serve
requests but is never on the path of a request, so it can stop, restart
or be upgraded while they carry on. Start it again and it takes them over
where they are:

```
muructl daemon start
```

`muructl events` ends with `release.adopted` for each release that was
running.

## Clean up

Stop the loop with Ctrl-C, then stop the daemon and everything it runs:

```
muructl daemon stop --shutdown
```

The state store remembers `hello` and its releases, so the next
`muructl daemon start` serves `r1` again.

## From here

- [Your first canary](first-canary.md) deploys a release to a share of the
  traffic first and lets the daemon decide whether to promote it.

- [The daemon](../howto/daemon.md) covers releases, retention, applying
  files and where the daemon keeps its files.

- [Running under systemd](../howto/systemd.md) runs the daemon as a
  service.
