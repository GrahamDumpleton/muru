# Run your first WSGI app

```{admonition} Status
:class: note

This tutorial covers development mode, which is what exists today. The
install step builds Muru from source, because the pre-releases on PyPI
predate it; that changes with the next pre-release.
```

In about ten minutes you will serve a small WSGI application with
`muructl run`, read what the terminal tells you, see what happens when the
application fails, and try watch reload, HTTPS and the dashboard.

## Install

Muru is two pieces: `muructl`, the command, and `muru`, the Python runtime
library that runs inside your application's interpreter. Until a pre-release
carries them, build both from the repository. You need a C toolchain, git,
[rustup](https://rustup.rs), [uv](https://docs.astral.sh/uv/) and
[just](https://github.com/casey/just); the
[contributing guide](../project/contributing.md) has the install commands for
macOS and Linux.

```
git clone https://github.com/GrahamDumpleton/muru
cd muru
just bootstrap          # Rust components, Python interpreters, the dev environment
just dev                # builds the muru extension into the repository's .venv
cargo build -p muructl  # the command, at target/debug/muructl
```

The rest of this tutorial uses the repository's virtualenv, so that the
runtime library is importable. Put `target/debug` on your PATH, or spell
out the path to `muructl` each time.

## A small application

Make a directory anywhere outside the repository and save this as `app.py`:

```python
def application(environ, start_response):
    body = f"Hello from {environ['PATH_INFO']}\n".encode()
    start_response("200 OK", [("Content-Type", "text/plain"), ("Content-Length", str(len(body)))])
    return [body]
```

Any WSGI callable works the same way: a Flask `app`, a Django
`get_wsgi_application()`, or a framework-free function like this one.

## Run it

```
muructl run app:application --python /path/to/muru/.venv/bin/python
```

The argument is `module:callable`. `--python` names the interpreter that has
the runtime library; once `muru` is installed in your own virtualenv, you
activate that and leave the flag out. The terminal shows:

```
muru.host[12345]: preloaded app:application in 2 ms
muru.host[12345]: worker 0 started as pid 12346
muru: serving app:application on http://127.0.0.1:8080 with 1 workers of 4 threads
muru: development mode: error pages show tracebacks, nothing is persisted and only loopback is served; production runs under the daemon, which arrives in phase 2
muru: dashboard on http://127.0.0.1:8081/
```

Three things happened. A host process imported your application once. It
forked a worker, which is a copy of the host with the application already
loaded. And the edge, the part of muructl that speaks HTTP, started
listening in front of the worker. Open http://127.0.0.1:8080/hello and the
terminal adds an access log line:

```
05:42:22.836 200 GET /hello 18B 0.7ms queue=0.2ms app=0.3ms worker=0 id=01M5P6WZBK740HFT2N80MAV4YV
```

That is the status, the method and path, the response size, the total
time, how long the request waited for a worker thread, how long your code
took, which worker served it, and the request id that every response also
carries in an `X-Request-ID` header. Lines your application prints come
through tagged with the worker that wrote them, so output from different
workers can be told apart.

Stop it with Ctrl-C. Workers finish the requests they are serving, then
everything exits.

## When the application fails

Change the application to raise:

```python
def application(environ, start_response):
    raise RuntimeError("not today")
```

Start it again and load the page. The browser shows an error page with the
traceback, because development mode shows tracebacks to clients; the
terminal shows the same traceback, tagged with the worker, and a `500` log
line with `reason=app_error`. Ask for JSON instead and you get a
`application/problem+json` document with the same details:

```
curl -H 'Accept: application/json' http://127.0.0.1:8080/
```

The worker is fine: an exception in your code is a failed request, never a
dead process.

## Reload on change

```
muructl run app:application --reload
```

Now fix the application and save. The terminal says what changed and that
it switched to a new release:

```
muru: reloading, app.py changed
muru.host[12400]: preloaded app:application in 2 ms
muru.host[12400]: worker 0 started as pid 12401
muru: switched to release r2
```

Each reload is a fresh host and worker; the old ones serve until the new
ones are ready, then stop. If the new code fails to import, the terminal
says so and the previous release keeps serving.

## More workers, more threads

```
muructl run app:application --processes 2 --threads 8
```

By default one worker with four threads serves requests, which is plenty
for a laptop. The [process models](../explanation/process-models.md) page
explains what processes and threads buy you and how the free-threaded
builds of Python change the picture.

## HTTPS

```
muructl run app:application --https
```

HTTPS appears on port 8443 next to HTTP. If you have
[mkcert](https://github.com/FiloSottile/mkcert) installed and have run
`mkcert -install`, the certificate is issued by your local authority and
browsers trust it without a warning. Otherwise muructl makes a self-signed
certificate and tells you how to trust it or install mkcert. Either way the
certificate is kept in your user cache directory and reused. Browsers speak
HTTP/2 over the TLS connection, which is one reason to use `--https`: it is
the only way to test HTTP/2, Secure cookies and anything that checks
`wsgi.url_scheme` without a reverse proxy in front.

## The dashboard

Open http://127.0.0.1:8081/ while the application runs. It shows the
workers, requests per second and latency over the last minute, the recent
requests and the recent output, refreshing every two seconds. It is served
on loopback only and is read-only.

## From here

- `muructl run --help` lists every option; the ones you will reach for first
  are `--access-log`, `--timeout` and `--hooks`.
- A project can carry its settings in a `[tool.muru]` table in
  pyproject.toml, so that `muructl run` with no arguments does the right
  thing; the [configuration reference](../project/design/appendices/c-configuration-reference.md)
  in the design specification lists the keys.
- [Preload and fork safety](../explanation/preload.md) explains the import
  check that runs before workers are forked, and the message you will see
  when an import opens a connection too early.
- Development mode serves loopback only and is not for production, which
  is the daemon's job in the next phase; the [roadmap](../project/roadmap.md)
  says when.
