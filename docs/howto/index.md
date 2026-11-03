# How-to guides

Recipes for specific tasks, assuming you already know the basics. The
[tutorials](../tutorials/index.md) are the place to learn them.

## Your application

- [Flask and Django](flask-and-django.md): serving either framework's
  WSGI application, static files included.
- [ASGI applications](asgi.md): ASGI over HTTP and WebSockets, lifespan
  and the event loop.
- [Starlette and FastAPI](starlette-and-fastapi.md): serving either
  framework's ASGI application.
- [Settings for muructl run](run-settings.md): every setting of a
  development run, from flags or the project's `[tool.muru]` table.
- [Interpreters and environments](interpreters.md): the interpreter, the
  environment Muru builds with uv, and the runtime library.
- [HTTPS in development](https.md): HTTPS and HTTP/2 with a local
  certificate.
- [Mounts](mounts.md): several WSGI and ASGI entrypoints in one release,
  by path.
- [Handing requests between WSGI and ASGI](handoff.md): choosing the
  interface in code, request by request.
- [gRPC services](grpc.md): gRPC beside WSGI and ASGI, or on its own.

## Deploying

- [The daemon](daemon.md): starting it and deploying applications.
- [Running under systemd](systemd.md): the daemon as a system service.
- [Per-application users](users.md): the edge and each application as a
  user of its own.
- [Secrets](secrets.md): credentials delivered to a release without
  appearing in its configuration.
- [Running commands](commands.md): management commands with a release's
  interpreter, environment and settings.
- [Database migrations](migrations.md): changing a schema while two
  releases share it.
- [In a container](containers.md): an image and `muructl serve` as its
  main process.
- [A Kubernetes Deployment](kubernetes.md): probes, rolling updates and
  sizing in a pod.
- [Upgrading Muru](upgrading.md): a new version of muructl and the
  daemon.

## Releases and traffic

- [Traffic between releases](traffic.md): switching, splitting, pinning
  and previewing.
- [Rollouts](rollouts.md): canaries and blue/green switches judged by
  gates.
- [Overlays](overlays.md): configuration laid over the routes by hand or
  on a schedule.
- [Client classes and rate limits](client-classes.md): classifying
  clients, limiting their rates and restricting methods.

## Routing and HTTP

- [Virtual hosts and TLS](virtual-hosts.md): several sites on one node
  by name, with a certificate each.
- [Behind a load balancer or proxy](behind-a-proxy.md): which proxies to
  trust and which of their headers to believe.
- [Static files](static-files.md): directories the edge serves itself.
- [File hints](file-hints.md): files an application hands to the edge to
  send.
- [Uploads](uploads.md): request bodies the edge receives and stores
  before the application sees them.
- [Internal redirects](internal-redirects.md): a request an application
  hands back to the edge to serve from elsewhere.
- [Custom error pages and maintenance mode](error-pages.md): the pages
  for errors the edge answers, and taking an application offline.
- [Authentication sub-requests](auth-subrequests.md): a decision from an
  application before the edge serves a route.
- [HTTP upstreams](upstreams.md): routes to HTTP servers Muru does not
  run.
- [Managed HTTP servers](managed-servers.md): gunicorn, uvicorn and
  others run as releases, for a cutover.
- [Response compression](compression.md): compressing responses at the
  edge.
- [Connections and keep-alive](connections.md): connection limits and
  idle timeouts.

## Workers

- [Worker groups](worker-groups.md): separate processes and threads for
  parts of an application.
- [Scaling](scaling.md): workers added and drained with demand, and the
  node's budget.
- [Timeouts](timeouts.md): what stops a request, or replaces a worker,
  that runs too long.
- [Worker recycling](recycling.md): replacing workers after a number of
  requests or a lifetime.
- [Memory limits](memory.md): replacing workers that grow too large.

## Observing and debugging

- [Logging](logging.md): application output, access logs and Python
  logging.
- [Metrics](metrics.md): Prometheus metrics from the edge and the
  workers.
- [Events and webhooks](events.md): every change of state, and sinks
  that receive them.
- [The dashboard](dashboard.md): charts, workers and actions in a
  browser.
- [Inspecting workers](debugging.md): stacks, profiles, memory and a
  debugger in running workers.

## Coding agents

- [Coding agents](coding-agents.md): an AI coding agent driving Muru
  over MCP during a coding session.

```{toctree}
:hidden:

flask-and-django
asgi
starlette-and-fastapi
run-settings
interpreters
https
mounts
handoff
grpc
daemon
systemd
users
secrets
commands
migrations
containers
kubernetes
upgrading
traffic
rollouts
overlays
client-classes
virtual-hosts
behind-a-proxy
static-files
file-hints
uploads
internal-redirects
error-pages
auth-subrequests
upstreams
managed-servers
compression
connections
worker-groups
scaling
timeouts
recycling
memory
logging
metrics
events
dashboard
debugging
coding-agents
```
