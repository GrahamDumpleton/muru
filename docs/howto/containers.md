# In a container

```{admonition} Status
:class: note

`muructl serve` runs one application in one container under Docker,
Compose, Podman or Kubernetes: configured from the image and the
environment, with probes and metrics on an admin port, logs on standard
output and a drain on `docker stop`. Sizing the worker count from the
container's limits, the pause a Kubernetes rolling update needs before
a pod stops, and guides for Compose and a Kubernetes Deployment are
still to come.
```

`muructl serve` runs one application as the container's main process: the
edge, the controller, the host and its workers, with nothing kept between
starts. In a Dockerfile it is one line:

```dockerfile
CMD ["muructl", "serve", "shop.wsgi:application"]
```

The application is served on port 8080 of every interface, and the
probes and metrics on port 9090. Reload is off, error pages carry no
tracebacks, the request body limit is 10 MB, and nothing is installed or
built when the container starts.

## An image

The image holds the application's environment and the runtime library,
both made when the image is built:

```dockerfile
FROM python:3.13-slim AS build
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN pip install --pre muructl
WORKDIR /app
COPY pyproject.toml uv.lock ./
RUN muructl build --deployment --deps-only --no-runtime --target /app/.venv
COPY . .
RUN muructl build --deployment --target /app/.venv --runtime-dir /opt/muru

FROM python:3.13-slim
COPY --from=build /app /app
COPY --from=build /opt/muru /opt/muru
COPY --from=build /usr/local/bin/muructl /usr/local/bin/muructl
WORKDIR /app
RUN useradd --uid 10001 app
USER app
EXPOSE 8080 9090
CMD ["muructl", "serve"]
```

The two `muructl build` steps install from `uv.lock` without resolving
anything:

- The first installs the dependencies alone, from `pyproject.toml` and
  `uv.lock`, before the source is copied in, so the layer is reused for
  as long as the dependencies stay the same. `--no-runtime` leaves the
  runtime library to the second step.

- The second installs the project itself into the same environment,
  compiles its bytecode, and unpacks the runtime library matching
  muructl's version and the environment's interpreter into `/opt/muru`,
  which the second stage copies.

A `.dockerignore` beside the Dockerfile keeps what the build makes, and
what it does not need, out of the copy of the source:

```
.venv
.git
__pycache__
*.pyc
muru.local.toml
```

`.venv` matters most: without it, a virtual environment on the machine
running the build is copied over the one the image built.

With no argument, `muructl serve` finds the application the way
`muructl run .` does, from the `[tool.muru]` table of the project in the
working directory, or in `--chdir`. Its interpreter is the first of:

1. the `python` setting;
2. the virtual environment `VIRTUAL_ENV` names;
3. `/app/.venv/bin/python`;
4. `python3` on the `PATH`.

The runtime library is the environment's own `muru` when it matches
muructl's version, or else the directory `muructl build --runtime-dir`
filled, `/opt/muru` by default.

## Nothing installed at start

`muructl serve` never builds an environment, installs an interpreter or
fetches the runtime library, and anything it runs has `UV_OFFLINE=1` and
`UV_PYTHON_DOWNLOADS=never` set. A missing interpreter, package or
runtime library stops the start with a message naming it and the build
step that provides it:

```
muru: error: no runtime library for /app/.venv/bin/python (cp313): build the image with muructl build --runtime-dir /opt/muru, or add muru 0.1.0a15 to the environment
```

## Settings

Every setting is a flag. The same setting can come from four other
places, each taking precedence over the one before:

1. the project's `[tool.muru]` table, as for a deployment;
2. its `[tool.muru.serve]` table (`[serve]` at the top level of a
   `muru.toml`), whose keys are the flags in snake case;
3. a file named by `--config` or `MURU_SERVE_CONFIG`, holding the same
   keys at its top level;
4. an environment variable per flag, `MURU_SERVE_` and the flag's name in
   upper snake case: `MURU_SERVE_LISTEN` for `--listen`,
   `MURU_SERVE_PROCESSES` for `--processes`.

A flag takes precedence over all of them. A list given at one level
replaces the list below it, and an environment variable holds a list
separated by commas. So an image sets its defaults in the project or in
its command, and a deployment changes one setting with an environment
variable:

```toml
[tool.muru.serve]
processes = 4
access_log_exclude = ["/static/*"]
```

```
docker run -e MURU_SERVE_PROCESSES=8 registry.example.com/shop
```

`muructl serve --print-config` prints the settings it would use, each
with where it came from, and exits.

| Flag | Default | Effect |
| --- | --- | --- |
| `ENTRYPOINT` or `PATH` | the working directory | a `module:callable`, or the project directory |
| `--chdir DIR` | | the directory to find the project in |
| `--config FILE` | | a settings file |
| `--listen ADDRESS` | `0.0.0.0:8080` | where the application is served |
| `--admin-listen ADDRESS` | `0.0.0.0:9090` | where the probes and metrics are served, or `off` |
| `--admin-status` | off | serve `/status` on the admin port too |
| `--min-ready N` | 1 | the workers ready in every mount for `/readyz` to pass |
| `--processes N` | `auto`, the CPU count | worker processes |
| `--threads N` | 4 | threads per WSGI worker |
| `--python PATH` | as above | the interpreter |
| `--access-log WHAT` | `on` | `on`, `errors` (4xx and 5xx responses and requests slower than `--slow`) or `off` |
| `--access-log-exclude PATTERN` | | paths left out of the access log, repeatable |
| `--log-format FORMAT` | `text` | `text`, or `json` for a log collector |
| `--timestamps` | off | the full UTC time in front of every line |
| `--run-dir DIR` | `/run/muru` | where everything written at run time goes (also `MURU_RUN_DIR`) |
| `--runtime-dir DIR` | `/opt/muru` | where the image holds the runtime library |
| `--spool-quota SIZE` | 1GB | the most request bodies spooled to disk at once |
| `--trusted-proxy ADDRESS` | `loopback` | a proxy whose forwarding headers are applied: an address, a CIDR range, or `loopback`, `private` or `unix`, repeatable; see [behind a proxy](behind-a-proxy.md) |

`--interface`, `--factory`, `--sync-threads`, `--concurrency`,
`--preload`, `--hooks`, `--timeout`, `--max-body-size`, `--max-requests`,
`--max-lifetime`, `--memory`, the connection limits, `--static` and
`--no-compression` work as they do for [`muructl run`](run-settings.md).

## Probes and metrics

The admin port answers the questions a container platform asks, apart
from the application's port, so a route to the application never
reaches it and a busy application never delays it:

| Path | Answers 200 when | Use it for |
| --- | --- | --- |
| `/livez` | the edge responds and the host is alive; a pod whose workers are all busy is alive | a liveness probe |
| `/readyz` | at least `--min-ready` workers are ready in every mount, and the container is not stopping | a readiness probe |
| `/startupz` | the application has loaded and the first workers are ready | a startup probe |
| `/metrics` | always, with the [metrics](metrics.md) in Prometheus text format | scraping |
| `/status` | with `--admin-status`: the release, its workers and groups, and the settings worked out, as JSON | looking at a running container |

The port answers anyone who can reach it. In Kubernetes a NetworkPolicy
limits who that is. It must listen on an address the kubelet can reach,
so `127.0.0.1` stops the probes from working.

`muructl probe ready` asks the same question inside the container and
exits 0 or 1, so an image with no curl has a health check, with the
admin port open or closed (`live` and `startup` ask the other two):

```dockerfile
HEALTHCHECK --interval=5s CMD ["muructl", "probe", "ready"]
```

In a Kubernetes container spec:

```yaml
ports:
  - {name: http, containerPort: 8080}
  - {name: admin, containerPort: 9090}
startupProbe:
  httpGet: {path: /startupz, port: admin}
  periodSeconds: 2
  failureThreshold: 60
livenessProbe:
  httpGet: {path: /livez, port: admin}
readinessProbe:
  httpGet: {path: /readyz, port: admin}
  periodSeconds: 2
```

## Logs

Everything goes to standard output: the application's output tagged with
its worker, Muru's own lines, and the access log. Lines have no time in
front, since the container runtime records one for each line; access log
lines start with the request's time. `docker logs -t` and `kubectl logs
--timestamps` show the runtime's. `--timestamps` puts the full UTC time
in front of every line, for a supervisor that adds none.

`--log-format json` writes one JSON object per line instead, for a log
collector, each with its `time`, a `kind` (`access`, `app` for the
application's output, or `muru`), and the line's fields:

```json
{"time":"2026-11-02T05:42:22.836Z","kind":"app","source":"muru.worker[0]","message":"WARNING demo: warned"}
```

## Files it writes

Everything `muructl serve` writes goes under one directory, `/run/muru`
unless `--run-dir` or `MURU_RUN_DIR` names another: the admin socket, the
workers' sockets, and request bodies too large to hold in memory, up to
`--spool-quota`, beyond which an upload is refused with 507. It assumes no
user account for its user ID and creates what it writes group-writable,
so it runs under whatever user ID the platform assigns with group 0, as
OpenShift does. With a read-only root filesystem, that directory and
`/tmp` are the only writable mounts it needs:

```yaml
securityContext:
  runAsNonRoot: true
  readOnlyRootFilesystem: true
  allowPrivilegeEscalation: false
  capabilities: {drop: ["ALL"]}
  seccompProfile: {type: RuntimeDefault}
volumeMounts:
  - {name: run, mountPath: /run/muru}
  - {name: tmp, mountPath: /tmp}
```

If the directory cannot be written, `muructl serve` stops at once and says
which volume it needs.

## Inside the container

`muructl` inside the container talks to `muructl serve` over the admin
socket in the run directory, so the commands that read or adjust a
running application work through `docker exec` or `kubectl exec`:

```
docker exec shop muructl status
docker exec shop muructl logs --access --follow
docker exec shop muructl debug stacks
docker exec shop muructl restart
```

Only someone who may exec into the container can use them. Commands that
manage releases, hosts or the daemon say they need the daemon.

## Stopping

muructl is the container's process 1 and runs as an init of its own, as
`tini` would, so nothing in the container is left a zombie. SIGTERM,
which `docker stop` and Kubernetes send, drains: `/readyz` starts failing,
the listener closes, requests in flight finish, and the container exits
0. A drain that runs past the platform's grace period (10 seconds for
`docker stop`, 30 for Kubernetes) is cut short by SIGKILL, so give an
application with long requests a longer one, with `stop_grace_period` in
Compose or `terminationGracePeriodSeconds` in Kubernetes. An image that
brings `tini` or runs under `docker run --init` sets `MURU_NO_INIT=1`.
