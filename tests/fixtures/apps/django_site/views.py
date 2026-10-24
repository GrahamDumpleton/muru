"""Views for the Django fixture."""

from __future__ import annotations

import hashlib
import os
import time
from collections.abc import Iterator

from django.core.exceptions import PermissionDenied
from django.http import (
    FileResponse,
    HttpRequest,
    HttpResponse,
    HttpResponseRedirect,
    JsonResponse,
    StreamingHttpResponse,
)
from django.views.decorators.csrf import csrf_exempt


def index(request: HttpRequest) -> HttpResponse:
    return HttpResponse("Hello from Django\n", content_type="text/plain")


def as_json(request: HttpRequest) -> HttpResponse:
    return JsonResponse({"pid": os.getpid(), "path": request.path, "args": request.GET.dict()})


@csrf_exempt
def echo(request: HttpRequest) -> HttpResponse:
    return HttpResponse(
        request.body, content_type=request.content_type or "application/octet-stream"
    )


@csrf_exempt
def form(request: HttpRequest) -> HttpResponse:
    return JsonResponse({"fields": request.POST.dict(), "files": sorted(request.FILES)})


@csrf_exempt
def upload(request: HttpRequest) -> HttpResponse:
    uploaded = request.FILES["file"]
    data = uploaded.read()
    return JsonResponse(
        {"name": uploaded.name, "size": len(data), "sha": hashlib.sha256(data).hexdigest()}
    )


def session(request: HttpRequest) -> HttpResponse:
    count = request.session.get("count", 0) + 1
    request.session["count"] = count
    return JsonResponse({"count": count})


def redirect_home(request: HttpRequest) -> HttpResponse:
    return HttpResponseRedirect("/")


def stream(request: HttpRequest) -> HttpResponse:
    def generate() -> Iterator[bytes]:
        for index in range(5):
            yield f"line {index}\n".encode()
            time.sleep(0.02)

    return StreamingHttpResponse(generate(), content_type="text/plain")


def error(request: HttpRequest) -> HttpResponse:
    raise RuntimeError("django-boom")


def forbidden(request: HttpRequest) -> HttpResponse:
    raise PermissionDenied


def sendfile(request: HttpRequest) -> HttpResponse:
    return FileResponse(open(__file__, "rb"), content_type="text/x-python")
