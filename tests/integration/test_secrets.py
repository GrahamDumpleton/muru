"""Secrets (design specification section 10.8): a release's environment
refers to secrets by store and key, the daemon resolves them when the
release starts from a file, env or exec store, delivers them as variables
or as files, and never records, exports or prints their values."""

from __future__ import annotations

import json
import stat
import subprocess
import sys
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon

PLAIN = support.FIXTURES


def env_of(daemon: Daemon, name: str) -> dict[str, object]:
    response = daemon.get(f"/env?name={name}")
    assert response.status == 200, response.body
    return dict(json.loads(response.body))


def test_secrets_are_resolved_from_every_kind_of_store_and_never_shown(tmp_path: Path) -> None:
    secrets_dir = tmp_path / "state" / "secrets"
    daemon = start_daemon(
        tmp_path,
        config_extra=(
            "[secrets.store.local]\nkind = 'file'\n\n"
            "[secrets.store.ci]\nkind = 'env'\n\n"
            "[secrets.store.vault]\nkind = 'exec'\n"
            "command = ['sh', '-c', 'printf value-for-%s {key}']\n"
            "timeout = '5s'\n"
        ),
        env={"CI_TOKEN": "token-from-the-daemon-environment"},
    )

    try:
        # The file store is managed without the daemon, by key, and the
        # value comes from standard input so it never sits in a command
        # line or a shell history.

        completed = daemon.muructl("secret", "set", "local", "db-password", stdin="hunter2\n")
        assert completed.stdout.strip() == "muru: secret local/db-password set"
        assert daemon.muructl("secret", "list", "local").stdout.split() == ["db-password"]
        written = secrets_dir / "local" / "db-password"
        assert written.read_text() == "hunter2"
        assert stat.S_IMODE(written.stat().st_mode) == 0o600
        assert stat.S_IMODE(written.parent.stat().st_mode) == 0o700

        assert daemon.deploy(
            "plain",
            PLAIN,
            "--entrypoint",
            "plain_wsgi:application",
            "--processes",
            "1",
            "--env",
            "SETTINGS=production",
            "--secret",
            "DB_PASSWORD=local:db-password",
            "--secret",
            "CI_TOKEN=ci:CI_TOKEN",
            "--secret",
            "API_KEY=vault:shop/api-key:file",
        ) == ("muru: release plain@r1 is ready and serving")

        assert env_of(daemon, "SETTINGS")["value"] == "production"
        assert env_of(daemon, "DB_PASSWORD")["value"] == "hunter2"
        assert env_of(daemon, "CI_TOKEN")["value"] == "token-from-the-daemon-environment"
        api_key = env_of(daemon, "API_KEY")
        assert api_key["value"] is None, "a file secret is not in the environment"
        assert api_key["content"] == "value-for-shop/api-key"
        file_path = Path(str(api_key["file"]))
        assert file_path.name == "API_KEY"
        assert stat.S_IMODE(file_path.stat().st_mode) == 0o400

        # What the daemon records, exports and prints names the store and
        # key, never the value.

        exported = daemon.muructl("config", "export").stdout
        assert "store = 'local'" in exported or 'store = "local"' in exported
        assert "key = 'db-password'" in exported or 'key = "db-password"' in exported
        assert "as = 'file'" in exported or 'as = "file"' in exported
        assert "SETTINGS = 'production'" in exported or 'SETTINGS = "production"' in exported
        assert "hunter2" not in exported
        assert "hunter2" not in daemon.muructl("events").stdout
        assert "hunter2" not in daemon.muructl("release", "list").stdout
        assert "hunter2" not in daemon.log()
        state = (tmp_path / "state").rglob("*")

        for path in state:
            if (
                path.is_file()
                and path.suffix in {"", ".db", ".sqlite"}
                and "secrets" not in path.parts
            ):
                assert b"hunter2" not in path.read_bytes(), path

        # Removing the key, then a release that needs it fails to start
        # with a message that names the key.

        assert "removed" in daemon.muructl("secret", "remove", "local", "db-password").stdout
        assert daemon.muructl("secret", "list", "local").stdout.strip() == "no secrets in local"
        failed = daemon.muructl(
            "deploy",
            "plain",
            str(PLAIN),
            "--entrypoint",
            "plain_wsgi:application",
            "--processes",
            "1",
            "--secret",
            "DB_PASSWORD=local:db-password",
            "--python",
            sys.executable,
            check=False,
        )
        assert failed.returncode != 0
        text = failed.stdout + failed.stderr
        assert "secret local/db-password: not found" in text
        assert daemon.releases("plain")[0] == {
            "release": "plain@r2",
            "state": "failed",
            "serving": "",
        }
        assert daemon.get("/").status == 200, "the first release keeps serving"
    finally:
        daemon.stop()


def test_declared_releases_carry_env_tables_and_unknown_stores_are_refused(
    tmp_path: Path,
) -> None:
    daemon = start_daemon(tmp_path, config_extra="[secrets.store.local]\nkind = 'file'\n")

    try:
        daemon.muructl("secret", "set", "local", "greeting", stdin="hello\n")
        config = tmp_path / "plain.toml"
        config.write_text(
            "[app.plain.release.r1]\n"
            f"source = '{PLAIN}'\n"
            "entrypoint = 'plain_wsgi:application'\n"
            f"python = '{sys.executable}'\n"
            "processes = 1\n\n"
            "[app.plain.release.r1.env]\n"
            "MODE = 'declared'\n"
            "GREETING = { secret = { store = 'local', key = 'greeting' } }\n"
            "NOTE = { secret = { store = 'local', key = 'greeting' }, as = 'file' }\n"
        )
        applied = daemon.muructl("apply", "-f", str(config), check=False)
        assert applied.returncode == 0, applied.stdout + applied.stderr
        assert env_of(daemon, "MODE")["value"] == "declared"
        assert env_of(daemon, "GREETING")["value"] == "hello"
        assert env_of(daemon, "NOTE")["content"] == "hello"
        assert daemon.muructl("config", "diff", "-f", str(config), check=False).returncode == 0

        config.write_text(
            "[app.plain.release.r2]\n"
            f"source = '{PLAIN}'\n"
            "entrypoint = 'plain_wsgi:application'\n"
            f"python = '{sys.executable}'\n"
            "processes = 1\n\n"
            "[app.plain.release.r2.env]\n"
            "TOKEN = { secret = { store = 'nowhere', key = 'x' } }\n"
        )
        refused = daemon.muructl("apply", "-f", str(config), check=False)
        assert refused.returncode != 0
        assert "no secret store named nowhere" in refused.stdout + refused.stderr
    finally:
        daemon.stop()


def test_the_secret_commands_need_a_file_store() -> None:
    muructl = support.muructl_path()
    assert muructl is not None
    completed = subprocess.run(
        [str(muructl), "secret", "list", "missing", "--config", "/nonexistent/muru.toml"],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert completed.returncode != 0
    assert "no secret store named missing" in completed.stderr
