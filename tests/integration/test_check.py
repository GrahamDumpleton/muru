"""`muructl check` (design specification section 6.8): the application
loaded in a throwaway host and validated, with nothing served and nothing
built."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

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
