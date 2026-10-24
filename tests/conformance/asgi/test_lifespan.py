"""The lifespan protocol: startup before readiness, shutdown on stop, failures."""

from __future__ import annotations

import pytest
from support import server as support


def test_shutdown_is_sent_on_stop() -> None:
    instance = support.start(
        "asgi_conformance:application", workers=1, threads=None, args=["--dashboard", "off"]
    )

    try:
        assert instance.get("/state").status == 200
    finally:
        instance.stop()

    assert "conformance: lifespan.shutdown received" in instance.log()


def test_startup_failed_stops_the_worker() -> None:
    with pytest.raises(RuntimeError) as failure:
        support.start(
            "asgi_lifespan_failing:application",
            workers=1,
            threads=None,
            args=["--dashboard", "off"],
        )

    assert "no database" in str(failure.value)
    assert "failed to start" in str(failure.value)


def test_application_without_lifespan_is_served() -> None:
    instance = support.start(
        "asgi_no_lifespan:application", workers=1, threads=None, args=["--dashboard", "off"]
    )

    try:
        assert instance.get("/").body == b"ok"
    finally:
        instance.stop()
