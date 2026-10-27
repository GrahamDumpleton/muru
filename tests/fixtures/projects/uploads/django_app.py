"""The Django recipe for offloaded uploads (section 4.11), as the uploads
guide gives it, in a project of one module: a middleware that turns the
stored files into request.FILES, and a view that reports the form and
the files.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

import django
from django.conf import settings

settings.configure(
    DEBUG=False,
    SECRET_KEY="uploads-fixture",
    ALLOWED_HOSTS=["*"],
    ROOT_URLCONF=__name__,
    MIDDLEWARE=[f"{__name__}.offloaded_uploads"],
)
django.setup()

from django.core.files.uploadedfile import UploadedFile  # noqa: E402
from django.core.handlers.wsgi import WSGIHandler  # noqa: E402
from django.http import HttpRequest, HttpResponse, JsonResponse  # noqa: E402
from django.urls import path  # noqa: E402
from django.utils.datastructures import MultiValueDict  # noqa: E402

from muru import uploads  # noqa: E402


def offloaded_uploads(
    get_response: Callable[[HttpRequest], HttpResponse],
) -> Callable[[HttpRequest], HttpResponse]:
    def middleware(request: HttpRequest) -> HttpResponse:
        found = uploads.files(request.META)

        if found:
            # The rewritten form first: Django parses it on first use and
            # would replace FILES with what it found there.

            _ = request.POST
            files: MultiValueDict[str, Any] = MultiValueDict()

            for upload in found:
                files.appendlist(
                    upload.field or "",
                    UploadedFile(upload.open(), upload.name, upload.content_type, upload.size),
                )

            request._files = files  # type: ignore[attr-defined]

        return get_response(request)

    return middleware


def describe(request: HttpRequest) -> JsonResponse:
    return JsonResponse(
        {
            "fields": request.POST.dict(),
            "files": {
                name: [stored.name, stored.content_type, stored.read().decode()]
                for name, stored in request.FILES.items()
            },
        }
    )


urlpatterns = [path("describe", describe)]

application = WSGIHandler()
