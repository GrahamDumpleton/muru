"""Entrypoints of `muructl run` (design specification sections 6.8 and 13.1).

The entrypoint comes from the command line or from a project's [tool.muru]
table, a factory is called per worker or in the host, and an ASGI
entrypoint, detected or declared, is served on the event loop.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from support import server as support

PROJECTS = support.ROOT / "tests" / "fixtures" / "projects"


def info(server: support.Server, path: str = "/") -> dict[str, object]:
    response = server.get(path)
    assert response.status == 200, response.body
    data: dict[str, object] = json.loads(response.body)
    return data


def start_project(*args: str, chdir: Path | None = None) -> support.Server:
    """Start muructl with a project path (or nothing) in place of an entrypoint."""

    return support.start(
        args[0] if args else None, args=args[1:], chdir=chdir, workers=None, threads=None
    )


def test_project_directory_supplies_the_entrypoint_and_settings() -> None:
    server = start_project(str(PROJECTS / "shop"))

    try:
        data = info(server)
        assert data["app"] == "shop"
        assert data["hooks_loaded"] == "yes", "the hooks module from the table was loaded"
        assert "muru: using [tool.muru] from" in server.log()
        assert "with 2 workers of 3 threads" in server.log()
        assert len(server.worker_pids()) == 2
    finally:
        server.stop()


def test_project_file_and_current_directory_work_too() -> None:
    server = start_project(str(PROJECTS / "shop" / "pyproject.toml"))

    try:
        assert info(server)["app"] == "shop"
    finally:
        server.stop()

    server = start_project(chdir=PROJECTS / "shop")

    try:
        assert info(server)["app"] == "shop"
    finally:
        server.stop()


def test_flags_override_the_table() -> None:
    server = start_project(str(PROJECTS / "shop"), "--processes", "1", "--threads", "2")

    try:
        assert "with 1 workers of 2 threads" in server.log()
    finally:
        server.stop()


def test_factory_is_called_in_each_worker_by_default() -> None:
    server = start_project(str(PROJECTS / "factory"), "--processes", "2")

    try:
        seen = {}

        for _ in range(20):
            data = info(server)
            seen[data["serving"]] = data
            if len(seen) == 2:
                break

        assert len(seen) == 2, "both workers answered"

        for pid, data in seen.items():
            assert data["created_in"] == pid, "each worker called the factory itself"
            assert data["calls"] == 1, "once per worker"
    finally:
        server.stop()


def test_factory_phase_host_calls_it_once_before_forking(tmp_path: Path) -> None:
    project = tmp_path / "project"
    project.mkdir()
    (project / "factory_site.py").write_text((PROJECTS / "factory" / "factory_site.py").read_text())
    (project / "muru.toml").write_text(
        'entrypoint = "factory_site:create_app"\nfactory = true\nfactory_phase = "host"\n'
    )
    server = start_project(str(project), "--processes", "2")

    try:
        data = info(server)
        assert data["created_in"] == server.host_pid()
        assert data["calls"] == 1
    finally:
        server.stop()


def test_factory_flag_on_the_command_line() -> None:
    server = support.start(
        "factory_site:create_app", chdir=PROJECTS / "factory", args=["--factory"]
    )

    try:
        assert info(server)["created_in"] != server.host_pid()
    finally:
        server.stop()


@pytest.mark.parametrize(
    ("entrypoint", "args"),
    [
        ("factory_site:asgi_app", []),
        ("factory_site:AsgiCallable", ["--factory", "--interface", "auto"]),
        ("factory_site:asgi_app", ["--interface", "asgi"]),
    ],
)
def test_asgi_entrypoints_are_served_on_the_event_loop(entrypoint: str, args: list[str]) -> None:
    server = support.start(entrypoint, chdir=PROJECTS / "factory", args=args)

    try:
        response = server.get("/")
        assert response.status == 200
        assert response.body == b"asgi\n"
        assert " as ASGI" in server.log()
    finally:
        server.stop()


def test_a_coroutine_declared_wsgi_is_refused() -> None:
    with pytest.raises(RuntimeError) as failure:
        support.start(
            "factory_site:asgi_app", chdir=PROJECTS / "factory", args=["--interface", "wsgi"]
        )

    assert "is declared WSGI but is a coroutine function" in str(failure.value)


def test_missing_entrypoint_is_a_clear_error(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError) as failure:
        support.start(chdir=tmp_path, workers=None, threads=None)
    assert "no entrypoint" in str(failure.value)
    (tmp_path / "pyproject.toml").write_text("[tool.muru]\nthreads = 2\n")
    with pytest.raises(RuntimeError) as failure:
        support.start(chdir=tmp_path, workers=None, threads=None)
    assert "has no entrypoint in its [tool.muru] table" in str(failure.value)
    (tmp_path / "pyproject.toml").write_text("[tool.muru]\nworkers = 2\n")
    with pytest.raises(RuntimeError) as failure:
        support.start(chdir=tmp_path, workers=None, threads=None)
    assert "workers" in str(failure.value), "an unknown key is named"
