# How-to guides

Recipes for specific tasks, assuming you already know the basics. These pages
are written as each capability lands.

Still to be written, as their features land:

- Migrating from mod_wsgi, gunicorn and uvicorn.
- Running under systemd.
- Uploads and authorised downloads (internal redirects), including upload adapters for Django, Flask and Starlette.
- Custom error pages and maintenance mode.
- Scaling, schedules and memory limits.
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
virtual-hosts
behind-a-proxy
worker-groups
mounts
https
timeouts
logging
metrics
events
```
