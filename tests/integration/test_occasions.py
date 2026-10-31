"""The development traceback page carries a seasonal theme when the run is
told to show one by name, and none when told off."""

from __future__ import annotations

import pytest
from support import server as support

HTML = {"Accept": "text/html,*/*;q=0.8"}


def test_a_named_occasion_themes_the_traceback_page() -> None:
    server = support.start("plain_wsgi:application", workers=1, args=["--easter-egg", "christmas"])

    try:
        response = server.get("/error", headers=HTML)
        assert response.status == 500
        assert response.header("Content-Type") == "text/html; charset=utf-8"
        body = response.body.decode()
        assert '<html lang="en" data-occasion="christmas">' in body
        assert '<div class="occasion" aria-hidden="true">' in body
        assert "<pre>" in body
        assert "deliberate failure" in body

        # Only the HTML page is themed: an API client's problem document
        # is as it always was.

        response = server.get("/error", headers={"Accept": "application/json"})
        assert response.status == 500
        assert "occasion" not in response.body.decode()
    finally:
        server.stop()


def test_a_replacing_occasion_leaves_the_page_bare() -> None:
    server = support.start(
        "plain_wsgi:application", workers=1, args=["--easter-egg", "css-naked-day"]
    )

    try:
        body = server.get("/error", headers=HTML).body.decode()
        assert 'data-occasion="css-naked-day"' in body
        assert "<style>" not in body
        assert "<pre>" in body
    finally:
        server.stop()


def test_off_shows_no_theme() -> None:
    server = support.start("plain_wsgi:application", workers=1, args=["--easter-egg", "off"])

    try:
        body = server.get("/error", headers=HTML).body.decode()
        assert '<html lang="en">' in body
        assert "occasion" not in body
        assert "<pre>" in body
    finally:
        server.stop()


def test_an_unknown_occasion_is_refused_with_the_list() -> None:
    with pytest.raises(RuntimeError) as failure:
        support.start("plain_wsgi:application", workers=1, args=["--easter-egg", "bogus"])

    text = str(failure.value)
    assert "bogus is not an occasion" in text
    assert "christmas" in text
    assert "sysadmin-day" in text
