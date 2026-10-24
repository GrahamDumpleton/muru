# How-to guides

Recipes for specific tasks, assuming you already know the basics. These pages
are written as each capability lands.

Still to be written, as their features land:

- Migrating from mod_wsgi, gunicorn and uvicorn.
- Running under systemd.
- Virtual hosts and TLS in production.
- Behind a load balancer or CDN (trusted proxies).
- Worker groups and mounts.
- Uploads and authorised downloads (internal redirects), including upload adapters for Django, Flask and Starlette.
- Custom error pages and maintenance mode.
- Scaling, schedules and memory limits.
- Log rotation.
- Containers and images.
- uv and Python versions.
- Event webhooks and OpenTelemetry.
- Configuration in version control (export and apply).
- Database migrations during overlapping releases.
- Upgrading Muru.

```{toctree}
:maxdepth: 1

flask-and-django
asgi
starlette-and-fastapi
daemon
https
timeouts
logging
```
