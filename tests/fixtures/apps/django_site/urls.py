"""URL patterns for the Django fixture."""

from __future__ import annotations

from django.urls import path

from django_site import views

urlpatterns = [
    path("", views.index),
    path("json", views.as_json),
    path("echo", views.echo),
    path("form", views.form),
    path("upload", views.upload),
    path("session", views.session),
    path("redirect", views.redirect_home),
    path("stream", views.stream),
    path("error", views.error),
    path("forbidden", views.forbidden),
    path("sendfile", views.sendfile),
]
