"""The steps of the Django tutorial, with the tutorial's own files.

docs/tutorials/first-django-site.md includes the files under
docs/tutorials/code/mysite. This test makes a project with
`startproject`, as the page does, lays those files over it, and runs it
through the page's steps, so a change that breaks one fails here rather
than for a reader.
"""

from __future__ import annotations

import http.cookiejar
import os
import re
import shutil
import subprocess
import sys
import urllib.parse
import urllib.request
from pathlib import Path

from support import server as support
from support.daemon import start_daemon

CODE = support.ROOT / "docs" / "tutorials" / "code" / "mysite"
PASSWORD = "a-long-test-password"


def manage(project: Path, *arguments: str, **env: str) -> None:
    subprocess.run(
        [sys.executable, "manage.py", *arguments],
        cwd=project,
        env={**os.environ, **env},
        check=True,
        capture_output=True,
    )


def make_project(base: Path) -> Path:
    """The project the page's first steps make, with its own files laid over it."""

    try:
        __import__("django")
    except ImportError:
        support.unavailable("Django is not installed")

    project = base / "mysite"
    project.mkdir()
    subprocess.run(
        [sys.executable, "-m", "django", "startproject", "mysite", "."], cwd=project, check=True
    )
    (project / "pyproject.toml").write_text('[tool.muru]\nentrypoint = "mysite.wsgi:application"\n')
    shutil.copy(CODE / "urls.py", project / "mysite" / "urls.py")
    shutil.copy(CODE / "production.py", project / "mysite" / "production.py")
    manage(project, "migrate")
    manage(
        project,
        "createsuperuser",
        "--noinput",
        DJANGO_SUPERUSER_USERNAME="admin",
        DJANGO_SUPERUSER_EMAIL="admin@example.com",
        DJANGO_SUPERUSER_PASSWORD=PASSWORD,
    )
    return project


def status(url: str) -> int:
    try:
        with urllib.request.urlopen(url) as response:
            code: int = response.status
            return code
    except urllib.error.HTTPError as error:
        return error.code


def log_in(base: str) -> str:
    """Log in to the admin through its form and return the page it lands on."""

    opener = urllib.request.build_opener(
        urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar())
    )
    form = opener.open(f"{base}/admin/login/").read().decode()
    match = re.search(r'name="csrfmiddlewaretoken" value="([^"]+)"', form)
    assert match is not None, form
    data = urllib.parse.urlencode(
        {
            "csrfmiddlewaretoken": match[1],
            "username": "admin",
            "password": PASSWORD,
            "next": "/admin/",
        }
    ).encode()
    request = urllib.request.Request(
        f"{base}/admin/login/", data=data, headers={"Referer": f"{base}/admin/login/"}
    )
    page: str = opener.open(request).read().decode()
    return page


def test_the_django_tutorial(tmp_path: Path) -> None:
    project = make_project(tmp_path)
    stylesheet = "/static/admin/css/base.css"

    # Run it from the project's [tool.muru] table: with Django's static
    # file patterns, the admin is styled and logging in works.

    server = support.start(None, workers=None, threads=None, chdir=project)

    try:
        base = f"http://127.0.0.1:{server.port}"
        assert status(f"{base}/") == 404  # the welcome page goes with the new URLs
        assert status(f"{base}{stylesheet}") == 200
        assert "Site administration" in log_in(base)
    finally:
        server.stop()

    # The same site over ASGI.

    server = support.start("mysite.asgi:application", workers=None, threads=None, chdir=project)

    try:
        base = f"http://127.0.0.1:{server.port}"
        assert "mysite.asgi:application is served as ASGI" in server.log()
        assert status(f"{base}{stylesheet}") == 200
        assert "Site administration" in log_in(base)
    finally:
        server.stop()

    # Production settings, collected static files declared in the project,
    # and a deploy: the static files come from the edge.

    production = {"DJANGO_SETTINGS_MODULE": "mysite.production", "DJANGO_SECRET_KEY": "x"}
    manage(project, "collectstatic", "--noinput", **production)
    assert (project / "static" / "admin" / "css" / "base.css").is_file()

    with (project / "pyproject.toml").open("a") as pyproject:
        pyproject.write(
            '\n[[tool.muru.static]]\npath = "/static/"\ndir = "static"\n'
            'cache = { max_age = "1h" }\n'
        )

    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")

    try:
        out = daemon.deploy(
            "mysite",
            project,
            "--env",
            "DJANGO_SETTINGS_MODULE=mysite.production",
            "--env",
            "DJANGO_SECRET_KEY=change-me",
        )
        assert out == "muru: release mysite@r1 is ready and serving"
        base = f"http://127.0.0.1:{daemon.port}"
        assert status(f"{base}{stylesheet}") == 200
        assert "Site administration" in log_in(base)
        access = daemon.muructl("logs", "--access").stdout
        assert f"200 GET {stylesheet}" in access
        assert "reason=static" in access
    finally:
        daemon.stop()
