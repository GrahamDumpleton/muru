"""The WSGI entrypoint of the Django fixture."""

from __future__ import annotations

import os

from django.core.wsgi import get_wsgi_application

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "django_site.settings")

application = get_wsgi_application()
