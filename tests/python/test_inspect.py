"""What debug inspect hides of the environment (design specification section 11.7)."""

from __future__ import annotations

import pytest

from muru import _inspect


@pytest.mark.parametrize(
    ("key", "value", "shown"),
    [
        ("DATABASE_URL", "postgres://shop:hunter2@db/shop", "postgres://shop:***@db/shop"),
        (
            "REDIS_URL",
            "rediss://default:s3cret@cache.example:6380/0?ssl_cert_reqs=none",
            "rediss://default:***@cache.example:6380/0?ssl_cert_reqs=none",
        ),
        ("AMQP_URL", "amqp://guest:gu@st@mq:5672/", "amqp://guest:***@mq:5672/"),
        (
            "CLUSTER",
            "redis://a:one@h1:6379,redis://b:two@h2:6379 http://c:three@h3",
            "redis://a:***@h1:6379,redis://b:***@h2:6379 http://c:***@h3",
        ),
        ("DATABASE_URL", "postgres://shop@db/shop", "postgres://shop@db/shop"),
        ("DATABASE_URL", "postgres://db/shop", "postgres://db/shop"),
        ("SENTRY_DSN", "https://abc123@o1.ingest.sentry.io/1", "***"),
        ("sentry_dsn", "https://abc123@o1.ingest.sentry.io/1", "***"),
        ("API_SECRET", "anything", "***"),
        ("DJANGO_SETTINGS_MODULE", "shop.settings", "shop.settings"),
        ("PATH", "/usr/bin:/bin", "/usr/bin:/bin"),
        ("HOME", "/Users/shop", "/Users/shop"),
        ("MOTD", "it's a URL: http://x:y@z and more", "it's a URL: http://x:***@z and more"),
        ("EMPTY", "", ""),
    ],
)
def test_a_password_in_a_url_is_hidden_and_the_rest_kept(key: str, value: str, shown: str) -> None:
    assert _inspect.redact(key, value) == shown
