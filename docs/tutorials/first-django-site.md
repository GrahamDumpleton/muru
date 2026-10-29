# Your first Django site

```{admonition} Status
:class: note

This tutorial assumes you have done [the first tutorial](first-wsgi-app.md)
and [deploy with the daemon and roll back](deploy-and-roll-back.md).
```

Django runs on Muru unchanged: the `wsgi.py` and `asgi.py` that
`startproject` writes are all Muru needs. What changes is everything
`manage.py runserver` used to do for you, static files above all.

In about twenty minutes you will make a Django project, serve it in
development with `muructl run`, get its admin styled and log in, serve
the same project over ASGI, and then deploy it to the daemon with
production settings, the edge serving the static files itself.

## A project

```
mkdir mysite && cd mysite
uv init --bare
uv add django muru
uv run django-admin startproject mysite .
uv run manage.py migrate
uv run manage.py createsuperuser
```

`createsuperuser` asks for a name, an email address and a password; you
will log in with them.

Tell Muru where the WSGI application is, so that neither `muructl run`
nor a deploy needs to be told each time. Add this to `pyproject.toml`:

```toml
[tool.muru]
entrypoint = "mysite.wsgi:application"
```

## Run it

```
muructl run --reload
```

With no arguments, `muructl run` reads the `[tool.muru]` table and uses
the project's `.venv`. Open http://127.0.0.1:8080/ and Django's welcome
page appears. Then open http://127.0.0.1:8080/admin/. The login page is
there, but unstyled, and the terminal says why:

```
muru.worker[0]: Not Found: /static/admin/css/base.css
```

`runserver` serves static files itself when `DEBUG` is on. No other
server does that for Django, Muru included: under `muructl run`, a
request for `/static/` reaches Django like any other and finds no URL
for it. In development, Django's own answer is to add its static file
patterns to the URLs. Change `mysite/urls.py` to this:

```{literalinclude} code/mysite/urls.py
:language: python
:emphasize-lines: 2,9
```

`--reload` sees the change, starts a new release and switches to it.
Reload the page and the admin is styled. Django's welcome page at `/`
gives way to its 404 page, as it does once a project has URLs of its
own. `staticfiles_urlpatterns()`
adds nothing when `DEBUG` is off, so it does no harm in production,
where something else serves the files, as you will see below.

Log in with the user you made. Sessions, the CSRF check on the login
form and the database all work as they do under `runserver`, because
Django is doing them; Muru only carries the requests.

Stop the server with Ctrl-C.

## The same site over ASGI

`startproject` also wrote `mysite/asgi.py`. Serve that instead:

```
muructl run mysite.asgi:application
```

```
muru.host[4242]: mysite.asgi:application is served as ASGI
```

The site is the same, admin and all. Under ASGI each worker runs an event
loop, Django's async views run on it, and its ordinary views run on the
worker's thread pool. Use whichever your project needs; the rest of this
tutorial uses WSGI. Stop it with Ctrl-C.

## Production settings

In production, `DEBUG` is off, the secret key comes from outside the
code, and static files are collected into one directory. Save this as
`mysite/production.py`:

```{literalinclude} code/mysite/production.py
:language: python
```

It takes everything from the development settings and changes what
production needs. Collect the static files with it:

```
DJANGO_SETTINGS_MODULE=mysite.production DJANGO_SECRET_KEY=x uv run manage.py collectstatic
```

That copies every application's static files, the admin's among them,
into `static/` in the project.

## Deploy it

Start the daemon if it is not running and deploy the project with the
production settings in its environment:

```
muructl daemon start
muructl deploy mysite . --env DJANGO_SETTINGS_MODULE=mysite.production --env DJANGO_SECRET_KEY=change-me
```

```
muru: release mysite@r1 is ready and serving
```

The entrypoint came from `[tool.muru]` and the interpreter from the
project's `.venv`. The admin is at http://127.0.0.1:8080/admin/ again,
unstyled again, since `DEBUG` is off and Django serves no static files.

## Static files from the edge

Give the site a host with two routes, one to the application and one for
`/static/` to the collected files. Save this as `site.toml` in the
project, since the `static` path is taken from the file's directory:

```{literalinclude} code/mysite/site.toml
:language: toml
```

and apply it:

```
muructl apply -f site.toml
```

```
muru: applied
  create-host     host site                created
  create-route    route site /             created
  create-route    route site /static/      created
```

The admin is styled once more, and the access log shows the difference
between the two kinds of request:

```
muructl logs --access
```

```
10:11:13.430 200 GET /static/admin/css/base.css 24kB 0.0ms host=127.0.0.1 reason=static id=01M60ZW3WNC5HY2QHF3EFRTZQS
10:11:13.436 200 GET /admin/login/ 4294B 21ms release=mysite@r1 host=127.0.0.1 queue=0.0ms app=21ms worker=0 id=01M60ZW3WWC5HY2QHF3EFRTZQS
```

The stylesheet was served by the edge from `static/`, with caching
headers and an `ETag`, compressed for a client that accepts it, without
reaching a worker. The login
page came from the release.

## Migrations

The daemon does not run `manage.py migrate`. Run it yourself, with the
production settings, before deploying a release that needs it. While a
deploy switches from one release to the next, both serve for a moment
against the same database, so a migration should leave the schema usable
by the release before it as well: add a column in one release, start
using it in the next, and remove the old one after that.

## Clean up

```
muructl daemon stop --shutdown
```

## From here

- [Flask and Django](../howto/flask-and-django.md) has more on running
  Django, error pages and HTTPS in development.

- [Static files](../howto/static-files.md) covers cache headers,
  precompressed files and fallbacks to the application.

- [Secrets](../howto/secrets.md) delivers the secret key from a store
  rather than the command line.

- [Your first canary](first-canary.md) works for a Django release just
  as it does for the small application there.
