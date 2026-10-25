# How-to guides

Recipes for specific tasks, assuming you already know the basics. These pages
are written as each capability lands.

Still to be written, as their features land:

- Migrating from mod_wsgi, gunicorn and uvicorn.
- Uploads, including upload adapters for Django, Flask and Starlette.
- Scaling and schedules.
- Log rotation.
- Containers and images.
- uv and Python versions.
- OpenTelemetry export.
- Database migrations during overlapping releases.
- Upgrading Muru.

```{toctree}
:maxdepth: 1

flask-and-django
asgi
starlette-and-fastapi
daemon
secrets
users
virtual-hosts
traffic
rollouts
behind-a-proxy
worker-groups
scaling
mounts
https
static-files
file-hints
uploads
internal-redirects
error-pages
auth-subrequests
upstreams
managed-servers
compression
timeouts
connections
recycling
memory
logging
debugging
metrics
events
systemd
dashboard
```
