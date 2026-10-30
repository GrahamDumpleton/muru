# Flask and Django

```{admonition} Status
:class: note

Both frameworks are part of Muru's own test suite, so what this page
describes is exercised on every change. The examples use development
mode; under the [daemon](daemon.md) the same entrypoint is deployed.
```

Nothing in either framework needs to know about Muru. Point `muructl run`
at the WSGI callable the framework already gives you.

## Flask

```
muructl run app:app
```

where `app.py` holds `app = Flask(__name__)`. With the application factory
pattern, name the factory and add `--factory`:

```
muructl run app:create_app --factory
```

The factory is called in each worker after the fork, so a database pool or
cache client it creates belongs to that worker. Without `--factory`, the
factory itself would be served and fail on every request, so Muru refuses
to start, since a callable taking no required arguments cannot be an
application. The `factory` key of the `[tool.muru]` table applies to the
table's own entrypoint only; an entrypoint named on the command line needs
`--factory`. Flask's own development server is not involved; `flask run`
and `muructl run` are alternatives.

Flask catches an exception a view raises, logs it through its logger,
which goes to standard error and so appears in the terminal tagged with
the worker, and answers with its own plain 500 page. Muru sees an
ordinary response, so the access log line has no `reason=`. To have
Muru's [error page](error-pages.md) instead, with the traceback in
development mode and `reason=app_error` in the access log, let the
exception reach Muru:

```python
app.config["PROPAGATE_EXCEPTIONS"] = True
```

## Django

```
DJANGO_SETTINGS_MODULE=mysite.settings muructl run mysite.wsgi:application
```

`mysite/wsgi.py` is the file `startproject` generated, which already
calls `get_wsgi_application()`. Setting `DJANGO_SETTINGS_MODULE` in the
environment is the usual way; the generated `wsgi.py` also sets a default
with `os.environ.setdefault`, so often nothing is needed.

Django imports its settings and builds the URL resolver when the
application is first called, not at import time, so preloading is clean.
If a settings module opens a database connection at import time, the
[import check](../explanation/preload.md) says so and the workers import
the application themselves instead.

Django also answers an unhandled exception itself, with its debug page
when `DEBUG = True` and its `handler500` view otherwise. Setting
`DEBUG_PROPAGATE_EXCEPTIONS = True` lets the exception reach Muru, which
answers with its own error page and records `reason=app_error`.

Static files: Django serves them only under `runserver`, so under Muru a
request for one reaches the application and finds no URL. The edge can
serve them instead, in development and production alike, from the
directory `collectstatic` fills, declared in the project (see the
[static files guide](static-files.md#an-applications-own-files)):

```toml
[[tool.muru.static]]
path = '/static/'
dir = 'static'
```

Or, in development, add Django's static file patterns to the URLs,
which serve them while `DEBUG = True` from each application's own
`static` directory, with no `collectstatic`, and add nothing otherwise:

```python
from django.contrib.staticfiles.urls import staticfiles_urlpatterns

urlpatterns += staticfiles_urlpatterns()
```

Under the daemon, the project's entry serves `STATIC_ROOT` after
`collectstatic`, as a [static route](static-files.md) at the site level
would, or use WhiteNoise. The
[Django tutorial](../tutorials/first-django-site.md) uses the URL
patterns in development and the project's entry in production.

Flask serves its package's `static` folder itself under any server.
Declaring it in the project hands those requests to the edge instead,
with the same `url_for('static', ...)` paths:

```toml
[[tool.muru.static]]
path = '/static/'
dir = 'shop/static'
```

## Both

- Put the settings in `[tool.muru]` of pyproject.toml and run with no
  arguments:

  ```toml
  [tool.muru]
  entrypoint = "mysite.wsgi:application"
  threads = 8
  ```

- The run reloads on changes to any `.py` file under the project; the
  first request after a change waits for the new workers rather than
  failing. `--reload-include '**/*.html'` adds templates to what is
  watched, and `--reload-exclude` takes files out of it; an exclude wins,
  so `--reload-exclude '**/*.py'` watches the included patterns alone.
  Virtualenvs, `__pycache__`, `.git`, editor files and what `.gitignore`
  names are never watched, and the start-up line says what is.
  `--no-reload` turns reloading off for a run, and the patterns can live
  in the project's run table with the port, HTTPS and the access log;
  [settings for muructl run](run-settings.md) has the table.
- `--https` gives the browser a TLS connection, which is needed to test
  `SESSION_COOKIE_SECURE`, `CSRF_COOKIE_SECURE` and `SECURE_SSL_REDIRECT`
  in Django, or `SESSION_COOKIE_SECURE` in Flask. Do not turn on HSTS in
  development: Muru warns if the application sends the header, because the
  browser would apply it to every server on localhost.
- Request bodies up to 100 MB are accepted in development; the framework's
  own limits (`DATA_UPLOAD_MAX_MEMORY_SIZE`, `MAX_CONTENT_LENGTH`) still
  apply inside the application.
