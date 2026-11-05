"""`muructl check` (design specification section 6.8): the application
loaded in a throwaway host and validated, with nothing served and nothing
built."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP


def muructl() -> Path:
    path = support.muructl_path()
    assert path is not None and support.extension_available(), (
        "muructl and the extension must be built"
    )
    return path


def check(*args: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    environment = {key: value for key, value in os.environ.items() if key != "VIRTUAL_ENV"}
    environment.update({"MURU_OFFLINE": "1", **(env or {})})

    return subprocess.run(
        [str(muructl()), "check", *args, "--python", sys.executable],
        capture_output=True,
        text=True,
        timeout=120,
        env=environment,
    )


def test_a_sound_project_passes(tmp_path: Path) -> None:
    source = tmp_path / "shop"
    shutil.copytree(SHOP, source, ignore=shutil.ignore_patterns("__pycache__"))
    completed = check(str(source))

    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert f"muru: check: [tool.muru] from {source / 'pyproject.toml'} is valid" in completed.stdout
    assert "muru: check: shop.wsgi:application is served as WSGI" in completed.stdout
    assert (
        "muru: check: hooks module shop.hooks defines on_load, on_reopen_logs" in completed.stdout
    )
    assert "muru: check: shop.wsgi:application passed" in completed.stdout
    assert "muru: serving" not in completed.stdout, "nothing is served"


def test_an_entrypoint_that_does_not_resolve_fails_with_the_error(tmp_path: Path) -> None:
    directory = tmp_path / "project"
    directory.mkdir()
    shutil.copy(support.FIXTURES / "plain_wsgi.py", directory / "app.py")
    (directory / "pyproject.toml").write_text("[tool.muru]\nentrypoint = 'app:nothing'\n")
    completed = check(str(directory))

    assert completed.returncode != 0
    assert "muru: check: error: AttributeError" in completed.stdout
    assert "the check failed" in completed.stderr


def test_the_import_check_warns_and_strict_fails(tmp_path: Path) -> None:
    completed = check("unsafe_import_wsgi:application", "--chdir", str(support.FIXTURES))

    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert "muru: check: warning: the import of unsafe_import_wsgi left" in completed.stdout

    strict = check("unsafe_import_wsgi:application", "--chdir", str(support.FIXTURES), "--strict")
    assert strict.returncode != 0
    assert "fail the check under --strict" in strict.stdout

    # As in the host, pre_fork runs before the check, so what it closes
    # is not reported.

    directory = tmp_path / "project"
    directory.mkdir()

    for name in ("preload_wsgi.py", "unsafe_import_wsgi.py", "hooks_reopen_socket.py"):
        shutil.copy(support.FIXTURES / name, directory / name)

    (directory / "pyproject.toml").write_text(
        "[tool.muru]\nentrypoint = 'unsafe_import_wsgi:application'\n"
        "hooks = 'hooks_reopen_socket'\n"
    )
    closed = check(str(directory), "--strict")
    assert closed.returncode == 0, closed.stdout + closed.stderr
    assert "warning" not in closed.stdout


def test_env_file_supplies_what_the_import_reads(tmp_path: Path) -> None:
    directory = tmp_path / "project"
    directory.mkdir()
    (directory / "app.py").write_text(
        "import os\n"
        "DATABASE_URL = os.environ['DATABASE_URL']\n"
        "def application(environ, start_response):\n"
        "    start_response('200 OK', [])\n"
        "    return [DATABASE_URL.encode()]\n"
    )
    (directory / "pyproject.toml").write_text("[tool.muru]\nentrypoint = 'app:application'\n")

    without = check(str(directory))
    assert without.returncode != 0
    assert "KeyError: 'DATABASE_URL'" in without.stdout

    env_file = tmp_path / "check.env"
    env_file.write_text("# placeholders for the check\nDATABASE_URL=sqlite:///placeholder\n")
    with_file = check(str(directory), "--env-file", str(env_file))
    assert with_file.returncode == 0, with_file.stdout + with_file.stderr
    assert "app:application passed" in with_file.stdout


MOUNT = "entrypoint = 'app:application'\n"


def assert_refused(tmp_path: Path, table: str, message: str) -> None:
    """Check a project whose table ends with ``table``; it fails with ``message``."""

    directory = tmp_path / "project"
    directory.mkdir()
    shutil.copy(support.FIXTURES / "plain_wsgi.py", directory / "app.py")
    (directory / "pyproject.toml").write_text(
        f"[tool.muru]\nentrypoint = 'app:application'\n\n{table}"
    )
    completed = check(str(directory))

    assert completed.returncode != 0
    assert message in completed.stderr, completed.stderr


@pytest.mark.parametrize(
    ("table", "message"),
    [
        (
            "[tool.muru.mounts.async_plain]\npath = '/api'\nentrypoint = 'app:application'\n",
            '"async_plain" is not a valid mount name',
        ),
        ("[tool.muru.groups.Reports]\npaths = ['/reports']\n", '"Reports" is not a valid worker'),
        ("[tool.muru.groups.default]\npaths = ['/reports']\n", '"default" is not a valid worker'),
        (
            "[tool.muru.groups.bots]\nmatch = { client_class = ['Bots'] }\n",
            'match.client_class: "Bots" is not a valid name',
        ),
        (
            "[tool.muru.groups.posts]\nmatch = { method = ['PO ST'] }\n",
            'match.method "PO ST" is not a method',
        ),
        ("[tool.muru.env]\nSHOP-URL = 'x'\n", 'env: "SHOP-URL" is not a variable name'),
    ],
)
def test_names_the_daemon_refuses_are_refused(tmp_path: Path, table: str, message: str) -> None:
    assert_refused(tmp_path, table, message)


@pytest.mark.parametrize(
    ("table", "message"),
    [
        ("[tool.muru.groups.reports]\nprocesses = 1\n", "has no paths and no match"),
        ("[tool.muru.groups.reports]\npaths = ['reports']\n", "does not start with /"),
        (
            "[tool.muru.groups.one]\npaths = ['/reports']\n\n"
            "[tool.muru.groups.two]\npaths = ['/reports']\n",
            "worker groups one and two ask the same of /reports",
        ),
        (
            "[tool.muru.groups.reports]\npaths = ['/reports']\nmounts = ['missing']\n",
            "worker group reports: no mount named missing",
        ),
        (f"[tool.muru.mounts.api]\npath = '/'\n{MOUNT}", '"/" is not a path below /'),
        (
            "[tool.muru.mounts.api]\npath = '/api'\nentrypoint = 'app'\n",
            "the entrypoint must be module:callable",
        ),
        (
            f"[tool.muru.mounts.one]\npath = '/api'\n{MOUNT}\n"
            f"[tool.muru.mounts.two]\npath = '/api/'\n{MOUNT}",
            "mount two: another mount has the path /api",
        ),
        ("[tool.muru.scaling]\nmode = 'spare'\n", "scaling: mode spare needs max_workers"),
        ("max_requests = 0\n", "max_requests must be at least 1"),
    ],
)
def test_tables_the_daemon_refuses_are_refused(tmp_path: Path, table: str, message: str) -> None:
    assert_refused(tmp_path, table, message)
