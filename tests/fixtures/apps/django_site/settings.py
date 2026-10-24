"""Settings for the Django fixture: sqlite in memory, no apps beyond the basics."""

from __future__ import annotations

SECRET_KEY = "fixture-only-not-secret"
DEBUG = False
ALLOWED_HOSTS = ["*"]
ROOT_URLCONF = "django_site.urls"
INSTALLED_APPS = [
    "django.contrib.contenttypes",
    "django.contrib.sessions",
]
MIDDLEWARE = [
    "django.middleware.common.CommonMiddleware",
    "django.contrib.sessions.middleware.SessionMiddleware",
    "django.middleware.csrf.CsrfViewMiddleware",
]
SESSION_ENGINE = "django.contrib.sessions.backends.signed_cookies"
DATABASES = {"default": {"ENGINE": "django.db.backends.sqlite3", "NAME": ":memory:"}}
DATA_UPLOAD_MAX_MEMORY_SIZE = 4 * 1024 * 1024
USE_TZ = True
